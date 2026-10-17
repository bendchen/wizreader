#!/usr/bin/env python3
"""s3stub.py —— 本地 S3 替身（**真 HTTP**，LWW e2e 用）

为什么要它
----------
`MemStore` 单测覆盖的是**判据**（闩 / 水位 / `synced_revision` / 冲突），但覆盖不到
**S3 适配层的真实 HTTP 行为**：`put_file_if` 的 `If-Match` 实际收发、412 的状态码还原、
`Content-MD5` / `x-amz-meta-*` 的往返、`ListObjectsV2` 的 XML 形状。
本替身就是为这一段存在的（定稿 §5.3 三个场景 + 我方补的两个）。

它只实现 rust-s3 0.37 在**本程序实际用到**的五个操作：
`PUT` / `GET` / `HEAD` / `DELETE` / `ListObjectsV2`（path-style、扁平键空间）。

**两条会让人白白调半天的铁律**（`docs/版本2.md` 第四篇 §26.6 记过一次）：
1. **响应里只许有一个 `Content-Length`**。给 `code >= 400` 的响应同时发两个 ⇒ HTTP 协议违规，
   reqwest 在**传输层**就报错 ⇒ rust-s3 根本看不到 412，条件写永远"假阴性"。
   本文件的 `_reply()` 是唯一出口，长度只在这里算。
2. `ListObjectsV2` 的 `<Contents>` 必须带 `<LastModified>`（serde 里是**非可选** String）
   与 `<Size>`，否则 rust-s3 解析直接报字段缺失。

测试钩子（确定性复现竞态，不靠抢时间窗）
----------------------------------------
`GET /__ctl/arm?manifest_412=1&object_412=0&dialect=unquoted&conditional=on`

- `manifest_412=N`：接下来 N 次 `manifest.db` 的 PUT **直接回 412**（等价于"读—改—写窗口里
  被别的写入端抢先"）。旧 e2e §14 用的就是这一招，理由：`sync_up` 是开头读 ETag、结尾才 PUT，
  窗口只有毫秒级，靠时序抢不可复现。
- `object_412=N`：接下来 N 次**带 `If-Match` 的笔记对象** PUT 回 412（D2 风险篇覆盖未命中）。
- `dialect=unquoted`：模仿 **Ceph RGW** 的方言 —— 把 `If-Match` 值与**不带引号**的 ETag 做字面
  比较，于是 RFC 的带引号形式永远 412（真云 2026-09-20 实测）。用它验 `S3Store` 的
  「引号 412 → 去引号重试 + 记住方言」。
- `conditional=off`：对任何条件 PUT 回 501（验降级为无条件 PUT）。

观察面：`GET /__ctl/state` 返回全部对象、计数与钩子；`GET /__ctl/object?key=K` 返回单对象的
md5/size/meta。

单独运行：`python3 s3stub.py --port 9000 [--log /tmp/s3stub.log]`
"""
from __future__ import annotations

import argparse
import base64
import collections
import hashlib
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlsplit

XMLNS = 'xmlns="http://s3.amazonaws.com/doc/2006-03-01/"'


def md5_hex(data: bytes) -> str:
    return hashlib.md5(data).hexdigest()


def xml_escape(s: str) -> str:
    return (
        s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;")
    )


class Stub:
    """对象库 + 计数器 + 钩子（线程安全）。"""

    def __init__(self) -> None:
        self.lock = threading.Lock()
        # key -> {"data": bytes, "meta": {k: v}, "mtime": float}
        self.objects: dict[str, dict] = {}
        self.counters: collections.Counter = collections.Counter()
        self.key_counters: dict[str, collections.Counter] = {}
        self.hooks = {
            "manifest_412": 0,
            "object_412": 0,
            "dialect": "rfc",  # rfc | unquoted
            "conditional": True,  # False ⇒ 条件 PUT 回 501
            # 内部状态：本次逻辑写已回过一次 412，正在等客户端的「去引号重试」
            "_episode": False,
        }
        # 事件流水（只留最后 200 条）：排查"为什么这里 412/200"用
        self.events: list[str] = []

    def ev(self, msg: str) -> None:
        self.events.append(msg)
        if len(self.events) > 200:
            del self.events[:-200]

    # ---------------- 内部 ----------------
    def _bump(self, key: str, op: str) -> None:
        self.counters[op] += 1
        self.key_counters.setdefault(key, collections.Counter())[op] += 1

    def key_counter(self, key: str, op: str) -> int:
        return self.key_counters.get(key, collections.Counter())[op]

    def revision(self) -> int | None:
        obj = self.objects.get("manifest.db")
        if obj is None:
            return None
        raw = obj["meta"].get("revision")
        try:
            return int(raw) if raw is not None else None
        except ValueError:
            return None

    def manifest_md5(self) -> str | None:
        obj = self.objects.get("manifest.db")
        return md5_hex(obj["data"]) if obj else None

    def state(self) -> dict:
        with self.lock:
            keys = []
            for k, o in sorted(self.objects.items()):
                keys.append(
                    {
                        "key": k,
                        "size": len(o["data"]),
                        "md5": md5_hex(o["data"]),
                        "etag": '"%s"' % md5_hex(o["data"]),
                        "meta": dict(o["meta"]),
                    }
                )
            return {
                "counters": dict(self.counters),
                "hooks": dict(self.hooks),
                "revision": self.revision(),
                "manifest_md5": self.manifest_md5(),
                "objects": keys,
                "events": list(self.events[-60:]),
            }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "wizs3stub/1.0"
    stub: Stub  # 由 make_server 注入

    # ---------------- 工具 ----------------
    def log_message(self, fmt: str, *args) -> None:  # 静音（需要时用 --log）
        log = getattr(self.server, "log_path", None)
        if log:
            with open(log, "a", encoding="utf-8") as f:
                f.write("[%s] %s %s\n" % (time.strftime("%H:%M:%S"), self.command, fmt % args))

    def _reply(
        self,
        code: int,
        body: bytes = b"",
        extra: dict | None = None,
        head_length: int | None = None,
    ) -> None:
        """**唯一**响应出口：Content-Length 只在这里写一次（见文件头铁律 1）。

        `head_length`：HEAD 响应不带 body，但 `Content-Length` 必须是**对象真实大小**
        （rust-s3 的 `HeadObjectResult.content_length` 就取它）。不传则用 body 长度。
        """
        self.send_response(code)
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        if "Content-Type" not in (extra or {}):
            self.send_header("Content-Type", "application/xml")
        length = head_length if head_length is not None else len(body)
        self.send_header("Content-Length", str(length))
        self.end_headers()
        if body and self.command != "HEAD":
            self.wfile.write(body)

    def _error(self, code: int, s3code: str, msg: str) -> None:
        body = (
            '<?xml version="1.0" encoding="UTF-8"?>'
            f"<Error><Code>{s3code}</Code><Message>{xml_escape(msg)}</Message></Error>"
        ).encode()
        self._reply(code, body)

    def _parse(self):
        parts = urlsplit(self.path)
        path = unquote(parts.path).lstrip("/")
        query = parse_qs(parts.query, keep_blank_values=True)
        if not path:
            return None, None, query
        if "/" in path:
            bucket, key = path.split("/", 1)
            return bucket, key, query
        return path, None, query

    def _read_body(self) -> bytes:
        te = (self.headers.get("Transfer-Encoding") or "").lower()
        if "chunked" in te:
            chunks = []
            while True:
                line = self.rfile.readline().strip()
                if not line:
                    break
                size = int(line.split(b";")[0], 16)
                if size == 0:
                    self.rfile.readline()
                    break
                chunks.append(self.rfile.read(size))
                self.rfile.readline()
            return b"".join(chunks)
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n) if n else b""

    def _obj_headers(self, data: bytes, meta: dict) -> dict:
        etag = '"%s"' % md5_hex(data)
        h = {
            "ETag": etag,
            "Accept-Ranges": "bytes",
            "Last-Modified": time.strftime("%a, %d %b %Y %H:%M:%S GMT", time.gmtime()),
            "Content-Type": "application/octet-stream",
        }
        for k, v in meta.items():
            h["x-amz-meta-" + k] = v
        return h

    @staticmethod
    def _if_match_ok(value: str, cur_md5: str | None, dialect: str) -> bool:
        """`If-Match` 判定。**两种方言严格互斥**，这是刻意的：

        - `rfc`（默认，= 真 S3 / MinIO 的口径）：**只认带引号形式**。去引号形式在这里**不匹配**
          ⇒ 客户端"引号 412 → 去引号重试"这一手在 rfc 服务端上也是 412（保守，绝不误判为成功）。
        - `unquoted`（= Ceph RGW 实测方言）：只认**不带引号**形式。

        为什么默认必须严格：若两种形式都放行，"注入一次 412"会被客户端的方言重试悄悄吃掉，
        条件写永远"假绿"（这正是 §5.3「并发注入」要抓的东西）。
        """
        if value.strip() == "*":
            return cur_md5 is not None
        if cur_md5 is None:
            return False
        if dialect == "unquoted":
            return value == cur_md5
        return value == '"%s"' % cur_md5

    # ---------------- 控制面 ----------------
    def _ctl(self) -> bool:
        parts = urlsplit(self.path)
        if not parts.path.startswith("/__ctl/"):
            return False
        cmd = parts.path[len("/__ctl/"):]
        q = parse_qs(parts.query, keep_blank_values=True)
        stub = self.stub
        if cmd == "state":
            body = json.dumps(stub.state(), ensure_ascii=False).encode()
        elif cmd == "arm":
            with stub.lock:
                for k in ("manifest_412", "object_412"):
                    if k in q:
                        stub.hooks[k] = int(q[k][0])
                if "dialect" in q:
                    stub.hooks["dialect"] = q["dialect"][0]
                if "conditional" in q:
                    stub.hooks["conditional"] = q["conditional"][0].lower() in ("on", "1", "true")
                stub.hooks["_episode"] = False
            body = json.dumps(stub.hooks).encode()
        elif cmd == "reset":
            with stub.lock:
                stub.counters.clear()
                stub.key_counters.clear()
            body = b'{"ok":true}'
        elif cmd == "object":
            key = q.get("key", [""])[0]
            with stub.lock:
                o = stub.objects.get(key)
                if o is None:
                    self._error(404, "NoSuchKey", key)
                    return True
                body = json.dumps(
                    {
                        "key": key,
                        "size": len(o["data"]),
                        "md5": md5_hex(o["data"]),
                        "meta": dict(o["meta"]),
                    },
                    ensure_ascii=False,
                ).encode()
        elif cmd == "purge":  # 清空对象（每个场景之间复位用）
            with stub.lock:
                stub.objects.clear()
            body = b'{"ok":true}'
        else:
            self._error(404, "NoSuchCommand", cmd)
            return True
        self._reply(200, body, {"Content-Type": "application/json"})
        return True

    # ---------------- S3 五个操作 ----------------
    def do_PUT(self) -> None:
        if self._ctl():
            return
        bucket, key, _ = self._parse()
        body = self._read_body()
        if not bucket or key is None:
            return self._error(400, "InvalidRequest", "bucket/key 缺失")
        hexmd5 = md5_hex(body)
        # Content-MD5（RFC 1864 的 base64）必须与收到的字节一致 —— 客户端 bug 的现场证据
        cm = self.headers.get("Content-MD5")
        if cm:
            want = base64.b64encode(bytes.fromhex(hexmd5)).decode()
            if cm.strip() != want:
                return self._error(400, "BadDigest", f"Content-MD5 {cm} ≠ {want}")
        meta = {}
        for hk, hv in self.headers.items():
            low = hk.lower()
            if low.startswith("x-amz-meta-"):
                meta[low[len("x-amz-meta-"):]] = hv
        meta.setdefault("content-md5", hexmd5)  # 与真 S3 一致：HEAD 回 content-md5 供续传比对
        if_match = self.headers.get("If-Match")
        if_none = self.headers.get("If-None-Match")
        is_manifest = key == "manifest.db" or key.endswith("/manifest.db")
        # 前缀为空时键形如 `native/notes/{GUID}`，**不带前导斜杠** —— 用 "/"+key 兜住
        is_note_obj = "/native/notes/" in "/" + key
        stub = self.stub
        with stub.lock:
            stub._bump(key, "put")
            cur = stub.objects.get(key)
            cur_md5 = md5_hex(cur["data"]) if cur else None
            dialect = stub.hooks["dialect"]
            if if_match or if_none:
                stub._bump(key, "cond_put")
            ev = ""
            if is_manifest:
                ev = f"PUT manifest if-match={if_match!r} if-none={if_none!r} "
            # 钩子①：清单 PUT 直接 412（"读—改—写窗口被抢先"的等价物）。
            #
            # **按「逻辑写」计数，不按 HTTP 请求计数**：客户端吃到 412 后会发一次「去引号」重试，
            # 那是**同一次**逻辑写。若把它也算一次注入，注入就被自己的重试吃掉了 ——
            # 症状是"注入了 3 次却只失败 2 轮"（本文件第一版就踩了）。故：带引号形式才扣减，
            # 紧随其后的去引号重试只回答 412（`_episode`）。
            if is_manifest and (
                stub.hooks["manifest_412"] > 0 or stub.hooks["_episode"]
            ):
                if if_none is not None:  # 首次提交（If-None-Match: *）视作一次逻辑写
                    stub.hooks["manifest_412"] = max(0, stub.hooks["manifest_412"] - 1)
                    stub.hooks["_episode"] = True
                elif if_match is not None and if_match.strip().startswith('"'):
                    stub.hooks["manifest_412"] = max(0, stub.hooks["manifest_412"] - 1)
                    stub.hooks["_episode"] = True
                elif stub.hooks["_episode"]:
                    stub.hooks["_episode"] = False
                else:  # 客户端已在「去引号」模式（上一轮探测到方言），一次算一次
                    stub.hooks["manifest_412"] = max(0, stub.hooks["manifest_412"] - 1)
                stub._bump(key, "manifest_412")
                stub.ev(ev + "→ 412（钩子 manifest_412）")
                return self._error(412, "PreconditionFailed", "钩子：manifest 提交被抢先")
            # 钩子②：风险篇对象条件覆盖直接 412（"旁置之后被改写"）
            if is_note_obj and if_match and stub.hooks["object_412"] > 0:
                stub.hooks["object_412"] -= 1
                stub._bump(key, "object_412")
                stub.ev(f"PUT note {key} if-match={if_match!r} → 412（钩子 object_412）")
                return self._error(412, "PreconditionFailed", "钩子：对象被改写")
            # 钩子③：服务端不支持条件写
            if (if_match or if_none) and not stub.hooks["conditional"]:
                stub.ev(ev + "→ 501（钩子 conditional=off）")
                return self._error(501, "NotImplemented", "钩子：条件写不可用")
            # 真判前置条件
            if if_match is not None and not self._if_match_ok(if_match, cur_md5, dialect):
                if dialect == "unquoted" and if_match.strip().startswith('"'):
                    stub._bump(key, "quoted_412")
                    stub.ev(ev + f"→ 412（unquoted 方言拒掉带引号形式，当前 {cur_md5}）")
                elif dialect == "rfc" and not if_match.strip().startswith('"'):
                    stub._bump(key, "unquoted_412")
                    stub.ev(ev + f"→ 412（rfc 方言拒掉去引号形式，当前 {cur_md5}）")
                else:
                    stub.ev(ev + f"→ 412（ETag 真的不符，当前 {cur_md5}）")
                return self._error(412, "PreconditionFailed", "If-Match 未命中")
            if if_none is not None:
                if if_none.strip() != "*":
                    return self._error(400, "InvalidRequest", "只支持 If-None-Match: *")
                if cur_md5 is not None:
                    stub.ev(ev + "→ 412（If-None-Match: * 但对象已存在）")
                    return self._error(412, "PreconditionFailed", "If-None-Match 未命中")
            stub.objects[key] = {"data": body, "meta": meta, "mtime": time.time()}
            if is_manifest:
                stub._bump("manifest.db", "manifest_put")
                stub.ev(ev + "→ 200（清单已写）")
            elif is_note_obj:
                stub._bump("note_obj", "put")
            return self._reply(200, b"", {"ETag": '"%s"' % hexmd5})

    def do_GET(self) -> None:
        if self._ctl():
            return
        bucket, key, query = self._parse()
        stub = self.stub
        if not bucket:
            return self._error(400, "InvalidRequest", "bucket 缺失")
        if key is None:
            return self._list(bucket, query)
        with stub.lock:
            stub._bump(key, "get")
            o = stub.objects.get(key)
            if o is None:
                return self._error(404, "NoSuchKey", key)
            data, meta = o["data"], dict(o["meta"])
            headers = self._obj_headers(data, meta)
        self._reply(200, data, headers)

    def do_HEAD(self) -> None:
        if self._ctl():
            return
        bucket, key, _ = self._parse()
        stub = self.stub
        if key is None:
            return self._reply(200, b"")  # HEAD bucket
        with stub.lock:
            stub._bump(key, "head")
            o = stub.objects.get(key)
            if o is None:
                return self._reply(404, b"")
            headers = self._obj_headers(o["data"], dict(o["meta"]))
            size = len(o["data"])
        self._reply(200, b"", headers, head_length=size)

    def do_DELETE(self) -> None:
        if self._ctl():
            return
        bucket, key, _ = self._parse()
        stub = self.stub
        with stub.lock:
            stub._bump(key or "", "delete")
            stub.objects.pop(key, None)
        self._reply(204, b"")

    def _list(self, bucket: str, query: dict) -> None:
        prefix = unquote(query.get("prefix", [""])[0])
        stub = self.stub
        with stub.lock:
            stub._bump("(list)", "list")
            keys = sorted(k for k in stub.objects if k.startswith(prefix))
            items = []
            for k in keys:
                o = stub.objects[k]
                items.append(
                    "<Contents>"
                    f"<Key>{xml_escape(k)}</Key>"
                    f"<LastModified>{time.strftime('%Y-%m-%dT%H:%M:%S.000Z', time.gmtime(o['mtime']))}</LastModified>"
                    f"<ETag>\"{md5_hex(o['data'])}\"</ETag>"
                    f"<Size>{len(o['data'])}</Size>"
                    "<StorageClass>STANDARD</StorageClass>"
                    "</Contents>"
                )
        body = (
            '<?xml version="1.0" encoding="UTF-8"?>'
            f"<ListBucketResult {XMLNS}>"
            f"<Name>{xml_escape(bucket)}</Name>"
            f"<Prefix>{xml_escape(prefix)}</Prefix>"
            "<MaxKeys>1000</MaxKeys>"
            "<IsTruncated>false</IsTruncated>"
            + "".join(items)
            + "</ListBucketResult>"
        ).encode()
        self._reply(200, body)


def make_server(port: int = 0, host: str = "127.0.0.1", log_path: str | None = None):
    stub = Stub()
    handler = type("BoundHandler", (Handler,), {"stub": stub})
    httpd = ThreadingHTTPServer((host, port), handler)
    httpd.daemon_threads = True
    httpd.log_path = log_path
    httpd.stub = stub
    return httpd, httpd.server_address[1], stub


def serve_in_thread(port: int = 0, log_path: str | None = None):
    httpd, port, stub = make_server(port, log_path=log_path)
    t = threading.Thread(target=httpd.serve_forever, name="s3stub", daemon=True)
    t.start()
    return httpd, port, stub


def main() -> None:
    ap = argparse.ArgumentParser(description="本地 S3 替身（WizReader LWW e2e）")
    ap.add_argument("--port", type=int, default=9000)
    ap.add_argument("--log", default=None)
    args = ap.parse_args()
    httpd, port, _ = make_server(args.port, log_path=args.log)
    print(f"s3stub listening on http://127.0.0.1:{port}  (bucket 任意，path-style)")
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
