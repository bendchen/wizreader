//! HTML 处理工具：正文纯文本抽取、指纹（P8）、标题净化（P3）、代码块物化（FR-08.7）

use regex::Regex;
use sha2::{Digest, Sha256};

/// 从 index.html 抽取正文纯文本（供索引与检索）
pub fn extract_text(html: &str) -> String {
    // 1. 去掉 style / script / 注释
    let re_style = Regex::new(r"(?is)<style[\s\S]*?</style>").unwrap();
    let re_script = Regex::new(r"(?is)<script[\s\S]*?</script>").unwrap();
    let re_comment = Regex::new(r"(?s)<!--[\s\S]*?-->").unwrap();
    let s = re_style.replace_all(html, " ");
    let s = re_script.replace_all(&s, " ");
    let s = re_comment.replace_all(&s, " ");
    // 2. 块级标签结束处转换行
    let re_block = Regex::new(r"(?i)</(p|div|li|tr|h[1-6]|br)\s*>|<br\s*/?>").unwrap();
    let s = re_block.replace_all(&s, "\n");
    // 3. 剥离所有标签
    let re_tag = Regex::new(r"(?s)<[^>]*>").unwrap();
    let s = re_tag.replace_all(&s, " ");
    // 4. 实体解码
    let s = decode_entities(&s);
    // 5. 压缩空白（保留换行）
    let re_line = Regex::new(r"[ \t\r\f]+").unwrap();
    let s = re_line.replace_all(&s, " ");
    let re_blank = Regex::new(r"\n\s*\n+").unwrap();
    let s = re_blank.replace_all(&s, "\n");
    s.trim().to_string()
}

/// 常见 HTML 实体解码（覆盖语料中出现的形态）
pub fn decode_entities(s: &str) -> String {
    let re_named = Regex::new(r"&(amp|lt|gt|quot|apos|nbsp|#\d+|#x[0-9a-fA-F]+);").unwrap();
    re_named
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
    let bad = Regex::new(r#"[\\/:*?"<>|]"#).unwrap();
    let mut t = bad.replace_all(title.trim(), "_").to_string();
    // 控制字符剔除
    let ctrl = Regex::new(r"[\x00-\x1f]").unwrap();
    t = ctrl.replace_all(&t, "").to_string();
    let t = t.trim().trim_end_matches('.').to_string();
    if t.is_empty() {
        return "untitled".into();
    }
    let truncated: String = t.chars().take(120).collect();
    truncated
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

/// 代码块物化（FR-08.7）：把形态 A 的隐藏 <textarea> 替换为静态 <pre><code>，
/// 使导出产物在裸浏览器中代码块可见。形态 B 不动。
/// 返回 (新 HTML, 替换块数)
pub fn materialize_code_blocks(html: &str) -> (String, usize) {
    let re = Regex::new(
        r#"(?is)(<div[^>]*class="[^"]*wiz-code-container[^"]*"[^>]*>)([\s\S]*?)<textarea[^>]*>([\s\S]*?)</textarea>([\s\S]*?)(</div>)"#,
    )
    .unwrap();
    let mut count = 0usize;
    let out = re.replace_all(html, |caps: &regex::Captures| {
        count += 1;
        let open = &caps[1];
        let head = caps[2].trim();
        let code_raw = decode_entities(&caps[3]);
        let tail = caps[4].trim();
        let mode = extract_data_mode(open);
        let lang = if mode.is_empty() {
            String::new()
        } else {
            format!(" class=\"language-{}\"", mode.to_lowercase())
        };
        format!(
            "{}{}<pre style=\"margin:0;overflow:auto;\"><code{}>{}</code></pre>{}</div>",
            head,
            if head.is_empty() { "" } else { "\n" },
            lang,
            html_escape(&code_raw),
            tail
        )
    });
    (out.to_string(), count)
}

fn extract_data_mode(open_tag: &str) -> String {
    let re = Regex::new(r#"(?i)data-mode="([^"]*)""#).unwrap();
    re.captures(open_tag)
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
        <body class="wiz-editor-body"><p>分布式文件系统</p><div>pm_api_url = 192.168.30.23<br>sefs</div></body></html>"#;
        let t = extract_text(html);
        assert!(t.contains("分布式文件系统"));
        assert!(t.contains("pm_api_url = 192.168.30.23"));
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
        assert!(out.contains("<code class=\"language-javascript\">"));
        assert!(out.contains("required_version = &quot;&gt;= 0.14&quot;") || out.contains("required_version"));
        assert!(!out.contains("textarea"));
    }
}
