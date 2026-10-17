#!/usr/bin/env python3
"""lww_e2e.py —— LWW 端到端（**真 HTTP 替身 + 多隔离 `WIZREADER_HOME`**）

对应 `docs/版本3-LWW选型定稿.md` §5.3 的三个场景，外加两个我方补的场景（§S4 对象级 412、
§S5 `If-Match` 方言）。为什么必须做：`MemStore` 单测覆盖的是**判据**，覆盖不到
`put_file_if` 的 `If-Match` 实际收发、412 状态码还原、`Content-MD5`/`x-amz-meta-*` 往返、
`ListObjectsV2` 的 XML 形状 —— 这些只有真 HTTP 能验。

| 场景 | 覆盖 | 关键断言 |
|---|---|---|
| `S1 双 writer 交替` | D1 自动对齐 + D2 风险篇 + 后提交者胜 | `retried≥1`、`overwritten_remote==1`、`_conflicts/{guid}_2.zip` == 失败方那一版的字节、云端对象 == 后提交者字节 |
| `S2 清单 412` | D1 对齐重试 | `retried==1`、两次清单 PUT（1 次 412 + 1 次 200）、闩清零、`sync.log` 固定措辞 |
| `S3 三轮全撞` | D1 耗尽 | 明确错误码、**云端清单字节一字未变**、**本地清单字节一字未变**（回滚）、闩保持 |
| `S4 对象级 412` | D2 跳过单篇（M5） | 只该篇不上行（其余照常提交）、该篇云端字节不变、闩保持、下一轮补上 |
| `S5 If-Match 方言` | Ceph 去引号适配 | 引号形式 412 → 去引号重试成功；调用方**看不到** 412（`retried==0`） |
| `S6 附件下行云端胜` | D5 | md5 分歧 ⇒ 覆盖本地且**真的发了 GET**；md5 相同 ⇒ **一个 GET 都不发** |

夹具（`/tmp` 内，源只读）
------------------------
基线库用**真实为知源**的一条子树建成（`build-md-library`，69 篇、无附件），每个场景复制一份，
每个"机器"一个隔离 `WIZREADER_HOME`（凭据走 `WIZREADER_SYNC_SECRET` 逃生通道，不碰钥匙串）。
S6 的附件行是**合成注入**的（CLI 导出路径产 0 附件行），只落在这个一次性库里。

用法
----
    python3 tools/tests/lww_e2e.py                 # 全部场景
    python3 tools/tests/lww_e2e.py --only S2,S3    # 单跑
    python3 tools/tests/lww_e2e.py --keep          # 保留 /tmp/wiz-lww-e2e 供排查

退出码：0 = 全部断言通过；1 = 有断言失败（逐条打印）。
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import sqlite3
import subprocess
import sys
import time
import traceback

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import s3stub  # noqa: E402  （同目录）

DEFAULT_CLI = os.path.abspath(
    os.path.join(HERE, "..", "..", "src-tauri", "target", "release", "wiz-cli")
)
DEFAULT_SOURCE = os.path.expanduser("~/.wiznote/boy2eye@126.com/data")
DEFAULT_INDEX_DIR = os.path.expanduser("~/.wizreader")
DEFAULT_FOLDER = "/boraydata/rdma-socket/"  # 69 篇、无附件、体积 ~1.6 MB
SECRET = "lww-e2e-secret"

RE_UP = re.compile(
    r"上传 (\d+) / 跳过 (\d+) / 超限 (\d+) / 冲突留存 (\d+) / 对齐重试 (\d+) / 覆盖远端版 (\d+)"
)
RE_DOWN = re.compile(r"下载 (\d+) / 移入回收站 (\d+) / 失败 (\d+)")


# ---------------------------------------------------------------- 基础设施


def md5_bytes(b: bytes) -> str:
    return hashlib.md5(b).hexdigest()


def md5_file(path: str) -> str:
    with open(path, "rb") as f:
        return md5_bytes(f.read())


def braced(guid: str) -> str:
    return guid if guid.startswith("{") else "{%s}" % guid


def note_key(guid: str) -> str:
    return "native/notes/%s" % braced(guid)


class Bench:
    """断言累加器（只记录，不中断 —— 一次跑完看到全部问题）。"""

    def __init__(self) -> None:
        self.items: list[tuple[str, bool, str]] = []
        self.scenario = ""

    def section(self, name: str) -> None:
        self.scenario = name
        print("\n" + "=" * 72)
        print(f"■ {name}")
        print("=" * 72)

    def check(self, name: str, ok: bool, detail: str = "") -> bool:
        self.items.append((f"{self.scenario} :: {name}", bool(ok), detail))
        mark = "✓" if ok else "✗"
        line = f"  {mark} {name}"
        if detail:
            line += f"   [{detail}]"
        print(line)
        return bool(ok)

    def eq(self, name: str, got, want) -> bool:
        return self.check(name, got == want, f"got={got!r} want={want!r}")

    def summary(self) -> int:
        bad = [i for i in self.items if not i[1]]
        print("\n" + "=" * 72)
        print(f"合计 {len(self.items)} 项断言：通过 {len(self.items) - len(bad)} / 失败 {len(bad)}")
        for name, _, detail in bad:
            print(f"  ✗ {name}   [{detail}]")
        print("=" * 72)
        return 0 if not bad else 1


class Machine:
    """一台"机器" = 一个库 + 一个隔离 `WIZREADER_HOME` + 一份 settings.json。"""

    def __init__(self, bench: Bench, work: str, name: str, port: int, role: str):
        self.bench = bench
        self.name = name
        self.lib = os.path.join(work, name, "lib")
        self.home = os.path.join(work, name, "home")
        os.makedirs(self.home, exist_ok=True)
        self.port = port
        self.role = role
        self.write_settings(role)

    # ---- 设置 ----
    def write_settings(self, role: str) -> None:
        self.role = role
        s = {
            "library_dir": self.lib,
            "font_size": 16,
            "theme": "system",
            "read_width": 860,
            "allow_remote": False,
            "sync": {
                "enabled": True,
                "role": role,
                "local_root": "",
                "endpoint": f"http://127.0.0.1:{self.port}",
                "bucket": "wiz",
                "prefix": "",
                "region": "us-east-1",
                "path_style": True,
                "access_key_id": "e2e",
                "credential_user": "e2e",
                "concurrency": 4,
                "auto_check_on_start": False,
                "initialized": True,
            },
        }
        with open(os.path.join(self.home, "settings.json"), "w", encoding="utf-8") as f:
            json.dump(s, f, ensure_ascii=False, indent=2)

    def clone_from(self, src: str) -> None:
        shutil.rmtree(self.lib, ignore_errors=True)
        shutil.copytree(src, self.lib)

    # ---- 执行 ----
    @property
    def env(self) -> dict:
        e = dict(os.environ)
        e["WIZREADER_HOME"] = self.home
        e["WIZREADER_SYNC_SECRET"] = SECRET
        return e

    def run(self, *args: str, expect_rc: int = 0) -> subprocess.CompletedProcess:
        cmd = [self.cli, *args]
        p = subprocess.run(cmd, env=self.env, capture_output=True, text=True)
        if expect_rc is not None and p.returncode != expect_rc:
            print(f"    ⚠ `{' '.join(args)}` rc={p.returncode}（期望 {expect_rc}）")
            print("      stdout:", p.stdout.strip()[-600:])
            print("      stderr:", p.stderr.strip()[-600:])
        return p

    cli = DEFAULT_CLI

    def cloud_init(self, role: str, expect_rc: int = 0):
        return self.run("cloud-init", role, expect_rc=expect_rc)

    def up(self, expect_rc: int = 0):
        return self.run("cloud-sync", "up", expect_rc=expect_rc)

    def down(self, expect_rc: int = 0):
        return self.run("cloud-sync", "down", expect_rc=expect_rc)

    def save_note(self, guid: str, text: str, tag: str, expect_rc: int = 0):
        f = os.path.join(self.home, f"new-{tag}.md")
        with open(f, "w", encoding="utf-8") as fh:
            fh.write(text)
        return self.run("save-note", guid, "--file", f, "--md", expect_rc=expect_rc)

    def verify_deep(self) -> subprocess.CompletedProcess:
        return self.run("verify-library", "--library-dir", self.lib, "--deep", expect_rc=0)

    # ---- 读清单 ----
    def db(self) -> sqlite3.Connection:
        return sqlite3.connect(f"file:{self.lib}/export.db?mode=ro", uri=True)

    def rev(self) -> int:
        with self.db() as c:
            row = c.execute("select value from meta where key='revision'").fetchone()
        return int(row[0]) if row else 0

    def latch(self, guid: str) -> tuple[int, int]:
        with self.db() as c:
            row = c.execute(
                "select dirty_info, dirty_data from note where guid=?", (guid,)
            ).fetchone()
        return (row[0], row[1]) if row else (-1, -1)

    def note_path(self, guid: str) -> str:
        with self.db() as c:
            row = c.execute("select exported_path from note where guid=?", (guid,)).fetchone()
        return os.path.join(self.lib, row[0])

    def note_count(self) -> int:
        with self.db() as c:
            return int(c.execute("select count(*) from note").fetchone()[0])

    def note_md5(self, guid: str) -> str:
        return md5_file(self.note_path(guid))

    def manifest_bytes(self) -> bytes:
        with open(os.path.join(self.lib, "export.db"), "rb") as f:
            return f.read()

    def log_since(self, offset: int) -> str:
        """只取本次同步新增的日志。

        ⚠ 必须**按字节**切：`offset` 是 `os.path.getsize`，而中文在 UTF-8 里 3 字节/字，
        按字符切会落在半行里 —— 断言"日志里有固定措辞"会随机假红（踩过一次）。
        """
        p = os.path.join(self.home, "sync.log")
        if not os.path.exists(p):
            return ""
        with open(p, "rb") as f:
            data = f.read()[offset:]
        return data.decode("utf-8", errors="replace")

    def log_size(self) -> int:
        p = os.path.join(self.home, "sync.log")
        return os.path.getsize(p) if os.path.exists(p) else 0

    def conflict_path(self, guid: str, rev: int) -> str:
        return os.path.join(self.lib, "_conflicts", f"{guid}_{rev}.zip")


def parse_up(stdout: str) -> dict:
    m = RE_UP.search(stdout)
    if not m:
        return {}
    keys = ["uploaded", "skipped", "oversized", "conflicts", "retried", "overwritten_remote"]
    d = {k: int(v) for k, v in zip(keys, m.groups())}
    d["manifest_uploaded"] = "清单已传" in stdout
    return d


def parse_down(stdout: str) -> dict:
    m = RE_DOWN.search(stdout)
    if not m:
        return {"downloaded": 0, "trashed": 0, "failures": 0, "none": "无变更" in stdout}
    return {"downloaded": int(m.group(1)), "trashed": int(m.group(2)), "failures": int(m.group(3))}


def cloud_md5(stub, key: str):
    with stub.lock:
        o = stub.objects.get(key)
        return md5_bytes(o["data"]) if o else None


def get_count(stub, key: str) -> int:
    return stub.key_counter(key, "get")


def cloud_manifest_row(stub, tmpdir: str, guid: str):
    """把云端清单取下来读某一行的 `(exported_md5, synced_revision, dirty_data)`。"""
    with stub.lock:
        o = stub.objects.get("manifest.db")
        data = o["data"] if o else b""
    p = os.path.join(tmpdir, "_cloud_manifest_ro.db")
    with open(p, "wb") as f:
        f.write(data)
    c = sqlite3.connect(f"file:{p}?mode=ro", uri=True)
    try:
        return c.execute(
            "select exported_md5, synced_revision, dirty_data from note where guid=?", (guid,)
        ).fetchone()
    finally:
        c.close()


def edit_text(tag: str) -> str:
    return f"# {tag}\n\n这是 LWW e2e 写入的正文（{tag}）。\n\n- 标记：`{tag}`\n"


def pick_notes(m: Machine, n: int = 2) -> list[str]:
    with m.db() as c:
        return [
            r[0]
            for r in c.execute(
                "select guid from note where origin='wiznote' order by exported_size asc limit ?", (n,)
            )
        ]


def inject_attachment(m: Machine, guid: str, name: str, data: bytes) -> str:
    """合成一条附件行 + 落盘文件（S6 专用；CLI 导出路径产 0 附件行）。

    表结构见 `manifest.rs`：`file_path` 相对源口径（`attachments/x` → 磁盘 `_attachments/x`）。
    同时把该篇 `has_attachment` 置 1，避免自检因标志与实际不符而报噪声。
    """
    rel = f"attachments/{name}"
    disk = os.path.join(m.lib, "_attachments", name)
    os.makedirs(os.path.dirname(disk), exist_ok=True)
    with open(disk, "wb") as f:
        f.write(data)
    c = sqlite3.connect(os.path.join(m.lib, "export.db"))
    c.execute(
        "insert or replace into attachment"
        "(file_path, display_name, size, tier, document_guid, source, cloud_key, dirty, synced_revision)"
        " values (?,?,?,?,?,?,NULL,1,0)",
        (rel, name, len(data), 1, guid, "wiznote-index"),
    )
    c.execute("update note set has_attachment=1 where guid=?", (guid,))
    c.commit()
    c.close()
    return disk


def fresh(work: str, name: str):
    d = os.path.join(work, name)
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d)
    httpd, port, stub = s3stub.serve_in_thread(0)
    return d, httpd, port, stub


def dump_events(stub) -> None:
    """`LWW_DEBUG=1` 时打印替身的事件流水（排查"这里为什么 412/200"）。"""
    if os.environ.get("LWW_DEBUG"):
        print("  --- stub events ---")
        for e in stub.state()["events"][-24:]:
            print("   ", e)


# ---------------------------------------------------------------- 场景


def s1_two_writers(bench: Bench, work: str, base: str) -> None:
    """双 writer 交替：A 上行 → B 上行（STALE ⇒ 自动对齐）→ A 再下行。"""
    bench.section("S1 双 writer 交替（D1 自动对齐 + D2 风险篇 + 后提交者胜）")
    w, httpd, port, stub = fresh(work, "s1")
    try:
        A = Machine(bench, w, "A", port, "writer")
        A.clone_from(base)
        n = A.note_count()
        rep = parse_up(A.cloud_init("writer").stdout)
        bench.eq(f"A 首次初始化：上传全部 {n} 篇", rep.get("uploaded"), n)
        bench.eq("A 首次初始化：云端 revision", stub.revision(), 1)

        B = Machine(bench, w, "B", port, "reader")
        p = B.cloud_init("reader")
        bench.eq("B 以 reader 全量下行篇数", parse_down(p.stdout).get("downloaded"), n)
        bench.eq("B 清单 revision 对齐远端", B.rev(), 1)

        note1, note2 = pick_notes(A, 2)
        # A 改一篇 → 上行 rev2
        A.save_note(note1, edit_text("A-ver"), "A1")
        rep = parse_up(A.up().stdout)
        bench.eq("A 二次上行只传 1 篇", rep.get("uploaded"), 1)
        bench.eq("A 二次上行没有重试（远端没动过）", rep.get("retried"), 0)
        bench.eq("云端 rev 前进到 2", stub.revision(), 2)
        a_ver = cloud_md5(stub, note_key(note1))
        bench.check("云端 note1 已是 A 的版本", a_ver == A.note_md5(note1), f"{a_ver}")

        # B 也改同一篇（基于 rev1）→ 上行必然先撞 STALE
        B.write_settings("writer")
        B.save_note(note1, edit_text("B-ver"), "B1")
        off = B.log_size()
        rep = parse_up(B.up().stdout)
        log = B.log_since(off)
        bench.eq("B 撞 STALE 后自动对齐重试 1 次", rep.get("retried"), 1)
        bench.check("sync.log 记了「提交未命中，下行对齐后重试」", "提交未命中，下行对齐后重试" in log)
        bench.eq("B 覆盖远端版计数", rep.get("overwritten_remote"), 1)
        bench.check("B 旁置了远端那一版（冲突留存 ≥1）", (rep.get("conflicts") or 0) >= 1, str(rep))
        bench.eq("云端 rev 前进到 3（提交者即最后写入者）", stub.revision(), 3)
        b_ver = B.note_md5(note1)
        bench.check("云端 note1 已是 B 的版本（后提交者胜）", cloud_md5(stub, note_key(note1)) == b_ver)
        cp = B.conflict_path(note1, 2)
        bench.check(
            "_conflicts/{guid}_2.zip 存在且字节 == A 那一版（30 天反悔窗口有保证）",
            os.path.exists(cp) and md5_file(cp) == a_ver,
            cp,
        )
        bench.eq("B 的闩已清零（这篇已发布）", B.latch(note1), (0, 0))

        # A 下行拿回 B 的版本
        p = A.down()
        bench.eq("A 下行拉到 1 篇", parse_down(p.stdout).get("downloaded"), 1)
        bench.check("A 的本地内容 == B 的版本", A.note_md5(note1) == b_ver)
        bench.eq("A 清单对齐 rev3", A.rev(), 3)
        bench.eq("A 库深度自检", A.verify_deep().returncode, 0)
        bench.eq("B 库深度自检", B.verify_deep().returncode, 0)
    finally:
        dump_events(stub)
        httpd.shutdown()


def s2_manifest_412(bench: Bench, work: str, base: str) -> None:
    """清单级 412：确定性注入「读—改—写窗口被抢先」。"""
    bench.section("S2 清单 412（并发注入 ⇒ D1 对齐重试成功后提交）")
    w, httpd, port, stub = fresh(work, "s2")
    try:
        M = Machine(bench, w, "M", port, "writer")
        M.clone_from(base)
        M.cloud_init("writer")
        note1, _ = pick_notes(M, 2)
        M.save_note(note1, edit_text("M-412"), "M1")
        off = M.log_size()
        put_before = stub.key_counter("manifest.db", "put")
        stub.hooks["manifest_412"] = 1
        p = M.up()
        rep = parse_up(p.stdout)
        log = M.log_since(off)
        bench.eq("上行最终成功", p.returncode, 0)
        bench.eq("对齐重试 1 次", rep.get("retried"), 1)
        bench.eq("清单已提交", rep.get("manifest_uploaded"), True)
        bench.eq("云端 rev 前进到 2", stub.revision(), 2)
        bench.check("sync.log 记了固定措辞", "提交未命中，下行对齐后重试" in log)
        bench.eq("闩已清零", M.latch(note1), (0, 0))
        bench.check(
            "本轮清单 PUT 共 3 次：注入 412（引号）+ 同一次逻辑写的去引号重试 412 + 最终成功",
            stub.key_counter("manifest.db", "put") - put_before == 3,
            str(stub.key_counter("manifest.db", "put") - put_before),
        )
        bench.eq("替身记录到 2 次 412（引号形式 + 同一次逻辑写的去引号重试）", stub.counters.get("manifest_412", 0), 2)
        bench.check(
            "云端清单字节 == 本地清单字节",
            stub.manifest_md5() == md5_bytes(M.manifest_bytes()),
        )
        bench.check("云端 note1 字节 == 本地 zip", cloud_md5(stub, note_key(note1)) == M.note_md5(note1))
        bench.eq("库深度自检", M.verify_deep().returncode, 0)
    finally:
        dump_events(stub)
        httpd.shutdown()


def s3_retry_exhausted(bench: Bench, work: str, base: str) -> None:
    """三轮全撞：三次都未命中 ⇒ 中止、云端与本地清单字节都不变、闩保持。"""
    bench.section("S3 三轮全撞（D1 重试耗尽 ⇒ 云端零改动 + 回滚铸版）")
    w, httpd, port, stub = fresh(work, "s3")
    try:
        M = Machine(bench, w, "M", port, "writer")
        M.clone_from(base)
        M.cloud_init("writer")
        note1, _ = pick_notes(M, 2)
        M.save_note(note1, edit_text("M-exhaust"), "M1")
        local_before = md5_bytes(M.manifest_bytes())
        cloud_before = stub.manifest_md5()
        obj_before = cloud_md5(stub, note_key(note1))
        off = M.log_size()
        put_before = stub.key_counter("manifest.db", "put")
        stub.hooks["manifest_412"] = 3
        p = M.up(expect_rc=1)
        log = M.log_since(off)
        bench.check("上行失败并给出明确错误码", "SYNC_MANIFEST_CONFLICT_RETRY_EXHAUSTED" in (p.stderr + p.stdout))
        bench.check("错误里写清「连续 3 次」", "连续 3 次提交未命中" in (p.stderr + p.stdout), p.stderr[-200:])
        bench.eq("云端清单 revision 未动", stub.revision(), 1)
        bench.check("云端清单字节一字未变", stub.manifest_md5() == cloud_before)
        bench.check("本地清单字节一字未变（铸版已回滚）", md5_bytes(M.manifest_bytes()) == local_before)
        bench.eq("闩保持置位（正文段；仅改正文 ⇒ dirty_info 本就是 0）", M.latch(note1), (0, 1))
        bench.eq("sync.log 记了 2 条「提交未命中」", log.count("提交未命中，下行对齐后重试"), 2)
        bench.check(
            "本轮清单 PUT 共 6 次（3 轮 × 引号形式 + 去引号重试）",
            stub.key_counter("manifest.db", "put") - put_before == 6,
            str(stub.key_counter("manifest.db", "put") - put_before),
        )
        bench.eq("3 轮注入全部命中（每轮 2 发：引号 + 去引号）", stub.counters.get("manifest_412", 0), 6)
        bench.check(
            "对象可能已先上云（记录：清单未提交前对象可超前）",
            True,
            f"before={obj_before} after={cloud_md5(stub, note_key(note1))}",
        )
        # 关掉钩子后下一轮应当补上（不自愈但要能续）
        p2 = M.up()
        bench.eq("钩子关掉后下一轮上行成功", p2.returncode, 0)
        bench.eq("下一轮 rev 前进到 2", stub.revision(), 2)
        bench.eq("闩最终清零", M.latch(note1), (0, 0))
    finally:
        dump_events(stub)
        httpd.shutdown()


def s4_object_412(bench: Bench, work: str, base: str) -> None:
    """对象级 412（D2）：只该篇本轮不上行，其余篇照常提交。"""
    bench.section("S4 对象级 412（风险篇跳过单篇，其余照常提交）")
    w, httpd, port, stub = fresh(work, "s4")
    try:
        A = Machine(bench, w, "A", port, "writer")
        A.clone_from(base)
        A.cloud_init("writer")
        note1, note2 = pick_notes(A, 2)
        # 顺序很重要：**B 必须先在 rev1 建库**，A 才改 note1 —— 否则 B 的基线本身就是 A 那一版，
        # 「远端 srev > 本端 srev」不成立、note1 根本不是风险篇（踩过：hook 一次都没命中）。
        B = Machine(bench, w, "B", port, "reader")
        B.cloud_init("reader")
        bench.eq("B 在 rev1 建库", B.rev(), 1)
        B.write_settings("writer")

        A.save_note(note1, edit_text("A-risk"), "A1")
        A.up()
        a_ver = cloud_md5(stub, note_key(note1))
        bench.check("云端 note1 是 A 的版本", a_ver == A.note_md5(note1))

        B.save_note(note1, edit_text("B-risk"), "B1")
        B.save_note(note2, edit_text("B-plain"), "B2")
        off = B.log_size()
        stub.hooks["object_412"] = 1
        p = B.up()
        rep = parse_up(p.stdout)
        log = B.log_since(off)
        bench.eq("上行整体成功（不因单篇未命中而中止）", p.returncode, 0)
        bench.eq("只传上 1 篇（note2）", rep.get("uploaded"), 1)
        bench.eq("覆盖远端版计数为 0（note1 没顶掉）", rep.get("overwritten_remote"), 0)
        bench.check(
            "失败项标明对象级 412",
            "SYNC_OBJECT_PRECONDITION_FAILED" in (p.stderr + p.stdout),
            p.stderr.strip()[-200:],
        )
        bench.check("sync.log 记了「本篇条件覆盖未命中，本轮跳过」", "本篇条件覆盖未命中，本轮跳过" in log)
        bench.check("云端 note1 仍是 A 的字节", cloud_md5(stub, note_key(note1)) == a_ver)
        bench.check("云端 note2 已是 B 的字节", cloud_md5(stub, note_key(note2)) == B.note_md5(note2))
        bench.eq("云端 rev 前进到 3（其余篇照常提交）", stub.revision(), 3)
        bench.eq("note1 闩保持置位", B.latch(note1), (1, 1))
        bench.eq("note2 闩已清零", B.latch(note2), (0, 0))
        cp = B.conflict_path(note1, 2)
        bench.check("旁置的仍是 A 那一版", os.path.exists(cp) and md5_file(cp) == a_ver, cp)

        # 边界登记（本轮 e2e 新发现）：对象级 412 后，本轮清单**照常发布**，而该篇的行是
        # 「本地版」（down 阶段回写的本地行：md5 = 本端、srev 未推进）⇒ 远端「清单行 md5」
        # 与「对象字节」短暂不一致（对象是对方那版）。下一轮上行成功后自愈。
        row = cloud_manifest_row(stub, w, note1)
        bench.check(
            "边界：本轮清单行的 md5 已是本端（对象仍是对方那版）⇒ 清单与对象短暂不一致，下轮自愈",
            row is not None and row[0] == B.note_md5(note1) and row[1] == 1,
            f"清单行={row} 本端 md5={B.note_md5(note1)[:8]} 云端对象={a_ver[:8]}",
        )

        rep = parse_up(B.up().stdout)
        bench.eq("下一轮补上：传 1 篇", rep.get("uploaded"), 1)
        # 下一轮**不再判风险篇**：上一轮发布的清单里该篇 `synced_revision` 仍是 1（没被推进），
        # 走的是"普通脏篇 ⇒ 无条件 PUT"。数据没有丢 —— A 那一版在第一轮已旁置 `_conflicts/`。
        bench.check(
            "下一轮不判风险篇（清单 srev 未推进 ⇒ 普通脏篇无条件 PUT）",
            rep.get("overwritten_remote") == 0,
            str(rep),
        )
        bench.check("旁置副本仍在（A 那一版没丢）", os.path.exists(cp) and md5_file(cp) == a_ver)
        bench.check("云端 note1 换成 B 的字节", cloud_md5(stub, note_key(note1)) == B.note_md5(note1))
        bench.eq("云端 rev 前进到 4", stub.revision(), 4)
        bench.eq("note1 闩最终清零", B.latch(note1), (0, 0))
        row2 = cloud_manifest_row(stub, w, note1)
        bench.check(
            "下一轮后清单行与对象一致（自愈）",
            row2 is not None and row2[0] == cloud_md5(stub, note_key(note1)),
            str(row2),
        )
        bench.eq("库深度自检", B.verify_deep().returncode, 0)
    finally:
        dump_events(stub)
        httpd.shutdown()


def s5_if_match_dialect(bench: Bench, work: str, base: str) -> None:
    """If-Match 方言：模仿 Ceph RGW「与不带引号 ETag 字面比较」。"""
    bench.section("S5 If-Match 方言（Ceph 去引号适配：引号 412 → 去引号成功）")
    w, httpd, port, stub = fresh(work, "s5")
    try:
        M = Machine(bench, w, "M", port, "writer")
        M.clone_from(base)
        stub.hooks["dialect"] = "unquoted"
        n = M.note_count()
        rep = parse_up(M.cloud_init("writer").stdout)
        bench.eq("初始化只走 If-None-Match: *（不受方言影响）", rep.get("uploaded"), n)
        note1, _ = pick_notes(M, 2)
        M.save_note(note1, edit_text("M-dialect"), "M1")
        put_before = stub.key_counter("manifest.db", "put")
        q412_before = stub.counters.get("quoted_412", 0)
        off = M.log_size()
        p = M.up()
        rep = parse_up(p.stdout)
        bench.eq("上行成功", p.returncode, 0)
        bench.eq("调用方看不到 412（内部方言重试不算 retried）", rep.get("retried"), 0)
        bench.eq("云端 rev 前进到 2", stub.revision(), 2)
        bench.eq(
            "清单 PUT 共 2 次（引号形式 1 次 + 去引号形式 1 次）",
            stub.key_counter("manifest.db", "put") - put_before,
            2,
        )
        bench.eq("替身确认收到了「带引号」那一次 412", stub.counters.get("quoted_412", 0) - q412_before, 1)
        bench.check("云端清单字节 == 本地清单字节", stub.manifest_md5() == md5_bytes(M.manifest_bytes()))
        log = M.log_since(off)
        bench.check(
            "没有把它误报成「条件 PUT 不可用」的降级",
            "条件 PUT 不可用" not in log,
            log.strip()[-200:],
        )
        bench.eq("库深度自检", M.verify_deep().returncode, 0)
    finally:
        dump_events(stub)
        httpd.shutdown()


def s6_attachment_down(bench: Bench, work: str, base: str) -> None:
    """附件下行（D5）：md5 分歧 ⇒ 云端胜；md5 相同 ⇒ 一个 GET 都不发。"""
    bench.section("S6 附件下行云端胜（D5：md5 分歧覆盖本地 / 相同不下载）")
    w, httpd, port, stub = fresh(work, "s6")
    try:
        A = Machine(bench, w, "A", port, "writer")
        A.clone_from(base)
        note1, _ = pick_notes(A, 2)
        n = A.note_count()
        v1 = b"attachment-v1\n"
        diskA = inject_attachment(A, note1, "e2e.txt", v1)
        rep = parse_up(A.cloud_init("writer").stdout)
        bench.eq(f"初始化把附件也传上去（uploaded = {n} 篇 + 1 附件）", rep.get("uploaded"), n + 1)
        akey = "native/attachments/e2e.txt"
        bench.check("云端附件字节 == v1", cloud_md5(stub, akey) == md5_bytes(v1))

        B = Machine(bench, w, "B", port, "reader")
        B.cloud_init("reader")
        diskB = os.path.join(B.lib, "_attachments", "e2e.txt")
        bench.check("B 全量下行拿到附件", os.path.exists(diskB) and md5_file(diskB) == md5_bytes(v1))

        # A 改附件内容 → 上行（附件差集按 md5 比，不依赖闩）
        v2 = b"attachment-v2-changed\n"
        with open(diskA, "wb") as f:
            f.write(v2)
        rep = parse_up(A.up().stdout)
        bench.eq("A 上传 1 个附件", rep.get("uploaded"), 1)
        bench.eq("云端 rev 前进到 2", stub.revision(), 2)
        bench.check("云端附件字节 == v2", cloud_md5(stub, akey) == md5_bytes(v2))

        gets_before = get_count(stub, akey)
        p = B.down()
        bench.eq("B 下行成功", p.returncode, 0)
        bench.check("D5 云端胜：B 的本地附件被覆盖成 v2", md5_file(diskB) == md5_bytes(v2))
        bench.eq("确实发了 1 次 GET（覆盖路径）", get_count(stub, akey) - gets_before, 1)

        # 只改笔记（附件不变）⇒ 附件一个 GET 都不该发
        A.save_note(note1, edit_text("A-att-note"), "A1")
        A.up()
        gets_before = get_count(stub, akey)
        q_before = stub.counters.get("head", 0)
        p = B.down()
        bench.eq("B 下行（笔记变化）成功", p.returncode, 0)
        bench.eq("附件 md5 相同 ⇒ 0 次 GET", get_count(stub, akey) - gets_before, 0)
        bench.check("附件仍做了 HEAD 比对（每轮一次，成本固定）", stub.counters.get("head", 0) > q_before)
        bench.check("B 的附件内容没被破坏", md5_file(diskB) == md5_bytes(v2))
        bench.check(
            "附件不旁置（_conflicts/ 里没有附件）",
            not any("e2e" in n for n in (os.listdir(os.path.join(B.lib, "_conflicts")) if os.path.isdir(os.path.join(B.lib, "_conflicts")) else [])),
        )
        bench.eq("B 库深度自检", B.verify_deep().returncode, 0)
    finally:
        dump_events(stub)
        httpd.shutdown()


SCENARIOS = {
    "S1": s1_two_writers,
    "S2": s2_manifest_412,
    "S3": s3_retry_exhausted,
    "S4": s4_object_412,
    "S5": s5_if_match_dialect,
    "S6": s6_attachment_down,
}


# ---------------------------------------------------------------- 入口


def ensure_base(args) -> str:
    base = args.base_lib
    if os.path.isdir(base) and os.path.exists(os.path.join(base, "export.db")) and not args.rebuild_base:
        print(f"复用基线库：{base}")
        return base
    print(f"构建基线库：{base}（源 {args.folder}，只读使用为知源）")
    shutil.rmtree(base, ignore_errors=True)
    os.makedirs(os.path.dirname(base), exist_ok=True)
    p = subprocess.run(
        [
            args.cli, "build-md-library", base,
            "--folder", args.folder,
            "--data-dir", args.data_dir,
            "--index-dir", args.index_dir,
        ],
        capture_output=True, text=True,
    )
    if p.returncode != 0:
        print(p.stdout[-2000:])
        print(p.stderr[-2000:])
        raise SystemExit(f"❌ 建基线库失败（rc={p.returncode}）")
    print("  " + [l for l in p.stdout.splitlines() if "导出完成" in l][0].strip())
    return base


def main() -> int:
    ap = argparse.ArgumentParser(description="WizReader LWW 端到端（真 HTTP 替身）")
    ap.add_argument("--cli", default=DEFAULT_CLI)
    ap.add_argument("--work", default="/tmp/wiz-lww-e2e")
    ap.add_argument("--base-lib", default=None)
    ap.add_argument("--folder", default=DEFAULT_FOLDER)
    ap.add_argument("--data-dir", default=DEFAULT_SOURCE)
    ap.add_argument("--index-dir", default=DEFAULT_INDEX_DIR)
    ap.add_argument("--only", default="")
    ap.add_argument("--rebuild-base", action="store_true")
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    args.base_lib = args.base_lib or os.path.join(args.work, "base-lib")

    if not os.path.isfile(args.cli):
        print(f"❌ 找不到 wiz-cli：{args.cli}（先在 src-tauri 下 cargo build --release -p wiz-cli）")
        return 1
    Machine.cli = args.cli

    os.makedirs(args.work, exist_ok=True)
    base = ensure_base(args)

    picked = [s.strip().upper() for s in args.only.split(",") if s.strip()] or list(SCENARIOS)
    bench = Bench()
    t0 = time.time()
    for name in picked:
        fn = SCENARIOS.get(name)
        if fn is None:
            print(f"⚠ 未知场景 {name}（可用：{','.join(SCENARIOS)}）")
            continue
        try:
            fn(bench, args.work, base)
        except Exception:
            bench.check(f"{name} 运行期异常", False, traceback.format_exc(limit=3).replace("\n", " | "))
    rc = bench.summary()
    print(f"耗时 {time.time() - t0:.1f}s；工作目录 {args.work}"
          + ("（已保留）" if args.keep else "（场景目录已保留，可加 --keep 说明）"))
    return rc


if __name__ == "__main__":
    sys.exit(main())
