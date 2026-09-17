//! HTML 处理工具：正文纯文本抽取、指纹（P8）、标题净化（P3）、代码块物化（FR-08.7）

use sha2::{Digest, Sha256};

regex_of!(re_style, r"(?is)<style[\s\S]*?</style>");
regex_of!(re_script, r"(?is)<script[\s\S]*?</script>");
regex_of!(re_comment, r"(?s)<!--[\s\S]*?-->");
regex_of!(re_block_end, r"(?i)</(p|div|li|tr|h[1-6]|br)\s*>|<br\s*/?>");
regex_of!(re_tag, r"(?s)<[^>]*>");
regex_of!(re_hspace, r"[ \t\r\f]+");
regex_of!(re_blank, r"\n\s*\n+");
regex_of!(re_entity, r"&(amp|lt|gt|quot|apos|nbsp|#\d+|#x[0-9a-fA-F]+);");
regex_of!(re_bad_name, r#"[\\/:*?"<>|]"#);
regex_of!(re_ctrl, r"[\x00-\x1f]");
regex_of!(re_cm_pre, r#"(?is)<pre[^>]*class="([^"]*)"[^>]*>([\s\S]*?)</pre>"#);
regex_of!(re_container_open, r#"(?is)<div[^>]*class="[^"]*wiz-code-container[^"]*"[^>]*>"#);
regex_of!(re_data_mode, r#"(?i)data-mode="([^"]*)""#);

/// 从 index.html 抽取正文纯文本（供索引与检索）
pub fn extract_text(html: &str) -> String {
    // 1. 去掉 style / script / 注释
    let s = re_style().replace_all(html, " ");
    let s = re_script().replace_all(&s, " ");
    let s = re_comment().replace_all(&s, " ");
    // 2. 块级标签结束处转换行
    let s = re_block_end().replace_all(&s, "\n");
    // 3. 剥离所有标签
    let s = re_tag().replace_all(&s, " ");
    // 4. 实体解码
    let s = decode_entities(&s);
    // 5. 压缩空白（保留换行）
    let s = re_hspace().replace_all(&s, " ");
    let s = re_blank().replace_all(&s, "\n");
    s.trim().to_string()
}

/// 常见 HTML 实体解码（覆盖语料中出现的形态）
pub fn decode_entities(s: &str) -> String {
    re_entity()
        .replace_all(s, |caps: &regex::Captures| {
            let m = &caps[1];
            match m {
                "amp" => "&".into(),
                "lt" => "<".into(),
                "gt" => ">".into(),
                "quot" => "\"".into(),
                "apos" => "'".into(),
                "nbsp" => " ".into(),
                _ => {
                    // 数字实体
                    let code = if let Some(hex) = m.strip_prefix("#x").or(m.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok()
                    } else if let Some(dec) = m.strip_prefix('#') {
                        dec.parse::<u32>().ok()
                    } else {
                        None
                    };
                    code.and_then(char::from_u32).map(|c| c.to_string()).unwrap_or_default()
                }
            }
        })
        .to_string()
}

/// 正文指纹：SHA-256 前 16 字节 hex（P8）
pub fn fingerprint(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.trim().as_bytes());
    let out = h.finalize();
    out[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

/// HTML 转义
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 标题净化（P3）：替换文件系统非法字符，截断至 120 字符
pub fn sanitize_title(title: &str) -> String {
    let mut t = re_bad_name().replace_all(title.trim(), "_").to_string();
    // 控制字符剔除
    t = re_ctrl().replace_all(&t, "").to_string();
    let t = t.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        return "untitled".into();
    }
    let truncated: String = t.chars().take(120).collect();
    truncated
}

/// 导出用目录路径（P3）：逐段净化，空段（如首尾 `/`）保留
pub fn sanitize_location(location: &str) -> String {
    location
        .split('/')
        .map(|seg| {
            if seg.is_empty() {
                seg.to_string()
            } else {
                sanitize_title(seg)
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// 导出用文件名（净化 + 重名时追加 GUID 前 8 位）
pub fn export_name(title: &str, guid: &str, used: &mut std::collections::HashSet<String>) -> String {
    let base = sanitize_title(title);
    let mut name = base.clone();
    if !used.insert(name.clone()) {
        let suffix: String = guid.trim_matches(['{', '}']).chars().take(8).collect();
        let head: String = base.chars().take(110).collect();
        name = format!("{}_{}", head, suffix);
        let mut i = 2;
        while !used.insert(name.clone()) {
            name = format!("{}_{}", head, i);
            i += 1;
        }
    }
    name
}

/// 一个「源码 textarea」形态的代码块（文档 P7 所称形态 A 的判据）
#[derive(Debug, Clone)]
pub struct CodeBlock {
    /// 容器 `data-mode`（语言标注，不可信，仅作 class 用）
    pub mode: String,
    /// textarea 内源码（已反转义）
    pub source: String,
    /// 是否已带 CodeMirror 序列化渲染结果（`<wiz_code_mirror>` 内有行）
    pub rendered: bool,
    /// textarea 元素在原文中的字节区间，便于精确替换
    pub range: (usize, usize),
}

regex_of!(re_textarea, r"(?is)<textarea\b([^>]*)>([\s\S]*?)</textarea>");

/// 是否为「隐藏的源码 textarea」：display:none / display: none / display:none !important
/// 且内容非空（CodeMirror 的空输入框 textarea 内容为空，自然排除）
fn is_source_textarea(attrs: &str, inner: &str) -> bool {
    if inner.trim().is_empty() {
        return false;
    }
    let a = attrs.to_ascii_lowercase();
    match a.find("display") {
        Some(i) => {
            let tail = &a[i..];
            tail[..tail.len().min(24)].contains("none")
        }
        None => false,
    }
}

/// CodeMirror 渲染区的代码行（每个 class 含整词 `CodeMirror-line` 的 `<pre>` 一行）。
/// 必须按 class **整词**匹配：测量用的 `CodeMirror-line-like`（内容是 xxx 串）不是代码行；
/// 带 `CodeMirror-measure` 的是测量占位，同样排除。
/// 渲染区的代码行（已反转义、去零宽、`&nbsp;` → 空格）
pub fn rendered_code_lines(mirror_inner: &str) -> Vec<String> {
    let out: Vec<String> = re_cm_pre()
        .captures_iter(mirror_inner)
        .filter(|c| {
            let toks: Vec<&str> = c[1].split_whitespace().collect();
            toks.iter().any(|t| *t == "CodeMirror-line") && !toks.iter().any(|t| t.contains("measure"))
        })
        .map(|c| {
            decode_entities(&re_tag().replace_all(&c[2], ""))
                .replace('\u{200b}', "")
                .replace('\u{a0}', " ")
        })
        .collect();
    out
}

/// 渲染区文本（逐行 \n 连接），用于与 textarea 源码逐字符核对
pub fn rendered_code_text(mirror_inner: &str) -> String {
    rendered_code_lines(mirror_inner).join("\n")
}

/// 渲染区是否真的有代码行（排除仅剩 CodeMirror-measure 占位的空镜像）
pub fn mirror_has_lines(mirror_inner: &str) -> bool {
    !rendered_code_lines(mirror_inner).is_empty()
}

/// 取某个代码块紧随其后的 `<wiz_code_mirror>` 内部内容
pub fn mirror_of(html: &str, after: usize) -> Option<&str> {
    let rest = html[after..].trim_start();
    let inner = rest.strip_prefix("<wiz_code_mirror>")?;
    let end = inner.find("</wiz_code_mirror>")?;
    Some(&inner[..end])
}

/// 归一化：CodeMirror 把制表符序列化为 4 空格、把 `&nbsp;` 当作缩进占位，
/// 比对源码与渲染态时须容忍这两类等价差异
pub fn normalize_code_text(s: &str) -> String {
    s.replace("\t", "    ").replace('\u{200b}', "").replace('\u{a0}', " ")
}

/// 扫描 HTML 中全部源码 textarea 代码块（含其渲染态标记）
pub fn code_blocks(html: &str) -> Vec<CodeBlock> {
    let mut out = Vec::new();
    let tags: Vec<(usize, usize, String)> = re_container_open()
        .find_iter(html)
        .map(|m| (m.start(), m.end(), extract_data_mode(m.as_str())))
        .collect();
    for cap in re_textarea().captures_iter(html) {
        let whole = cap.get(0).unwrap();
        let (attrs, inner) = (&cap[1], &cap[2]);
        if !is_source_textarea(attrs, inner) {
            continue;
        }
        let mode = tags
            .iter()
            .filter(|(_, e, _)| *e <= whole.start())
            .last()
            .map(|(_, _, m)| m.clone())
            .unwrap_or_default();
        // 渲染态判据：紧随其后的 <wiz_code_mirror> 内有真正的代码行
        let rendered = mirror_of(html, whole.end())
            .map(mirror_has_lines)
            .unwrap_or(false);
        out.push(CodeBlock {
            mode,
            source: decode_entities(inner),
            rendered,
            range: (whole.start(), whole.end()),
        });
    }
    out
}

/// 代码块物化（FR-08.7）：把「无渲染结果」的隐藏 <textarea> 替换为静态 <pre><code>，
/// 使导出产物在裸浏览器中代码块可见。
///
/// 实测校正（M4 巡检）：全库 1,533 个源码 textarea 中，1,496 个同时带有
/// CodeMirror 序列化渲染结果（`<wiz_code_mirror>`），只需保留渲染态；
/// 仅 37 个是真正的空白块，必须物化。若对已渲染态也做替换，代码会显示两遍。
/// 返回 (新 HTML, 物化块数)
pub fn materialize_code_blocks(html: &str) -> (String, usize) {
    let blocks = code_blocks(html);
    let mut out = String::with_capacity(html.len() + 1024);
    let mut pos = 0usize;
    let mut done = 0usize;
    for b in &blocks {
        if b.rendered {
            continue;
        }
        out.push_str(&html[pos..b.range.0]);
        let lang = if b.mode.is_empty() {
            String::new()
        } else {
            format!(" class=\"language-{}\"", html_escape(&b.mode.to_lowercase()))
        };
        out.push_str(&format!(
            "<pre style=\"margin:0;overflow-x:auto;white-space:pre;\"><code{}>{}</code></pre>",
            lang,
            html_escape(&b.source)
        ));
        pos = b.range.1;
        done += 1;
    }
    if done == 0 {
        return (html.to_string(), 0);
    }
    out.push_str(&html[pos..]);
    (out, done)
}

fn extract_data_mode(open_tag: &str) -> String {
    re_data_mode()
        .captures(open_tag)
        .map(|c| c[1].to_string())
        .unwrap_or_default()
}

/// 在 </body> 前注入 HTML 片段（宿主注入，仅限兼容层脚本/附件区）
pub fn inject_before_body_close(html: &str, snippet: &str) -> String {
    let lower = html.to_lowercase();
    match lower.rfind("</body>") {
        Some(idx) => format!("{}{}\n{}", &html[..idx], snippet, &html[idx..]),
        None => format!("{}\n{}", html, snippet),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_text() {
        let html = r#"<html><head><style>a{color:red}</style></head>
        <body class="wiz-editor-body"><p>分布式文件系统</p><div>pm_api_url = 192.0.2.1<br>sefs</div></body></html>"#;
        let t = extract_text(html);
        assert!(t.contains("分布式文件系统"));
        assert!(t.contains("pm_api_url = 192.0.2.1"));
        assert!(t.contains("sefs"));
        assert!(!t.contains("color:red"));
    }

    #[test]
    fn test_sanitize_title() {
        assert_eq!(sanitize_title("a/b|c:d"), "a_b_c_d");
        assert_eq!(sanitize_title("正常标题"), "正常标题");
    }

    #[test]
    fn test_materialize() {
        let html = r#"<div id="wiz_cm_1" class="wiz-code-container" data-mode="JavaScript"><textarea style="display:none;">terraform {
  required_version = "&gt;= 0.14"
}</textarea></div>"#;
        let (out, n) = materialize_code_blocks(html);
        assert_eq!(n, 1);
        assert!(out.contains("<code class=\"language-javascript\""));
        assert!(out.contains("required_version = &quot;&gt;= 0.14&quot;"));
        assert!(!out.contains("textarea"));
    }

    /// 已带 CodeMirror 序列化渲染结果的容器不得再物化，否则代码显示两遍
    #[test]
    fn test_materialize_skips_rendered() {
        let html = r#"<div class="wiz-code-container" data-mode="shell"><textarea style="display: none;">echo hi</textarea>
<wiz_code_mirror><div class="CodeMirror"><pre class="CodeMirror-line">echo hi</pre></div></wiz_code_mirror></div>"#;
        let blocks = code_blocks(html);
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].rendered);
        let (out, n) = materialize_code_blocks(html);
        assert_eq!(n, 0);
        assert_eq!(out, html, "已渲染态应原样输出");
    }

    /// 源码 textarea 与渲染态文本逐字符一致（容忍 tab→4 空格）
    #[test]
    fn test_rendered_matches_source() {
        let html = r#"<textarea style="display:none;">a	b
  c</textarea><wiz_code_mirror><pre class="CodeMirror-line">a    b</pre><pre class="CodeMirror-line">  c</pre></wiz_code_mirror>"#;
        let blocks = code_blocks(html);
        assert!(blocks[0].rendered);
        let mirror = &html[html.find("<wiz_code_mirror>").unwrap() + 17..html.find("</wiz_code_mirror>").unwrap()];
        assert_eq!(
            normalize_code_text(&rendered_code_text(mirror)),
            normalize_code_text(&blocks[0].source)
        );
    }

    #[test]
    fn test_sanitize_location() {
        assert_eq!(
            sanitize_location("/boraydata/rdma-socket/RDMA-400G测试/"),
            "/boraydata/rdma-socket/RDMA-400G测试/"
        );
        assert_eq!(sanitize_location("/a/b:c|d/"), "/a/b_c_d/");
    }
}
