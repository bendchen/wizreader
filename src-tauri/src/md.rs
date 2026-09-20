//! Markdown 库内格式（§20）：为知原生 HTML → Markdown，以及 Markdown → HTML。
//!
//! **为什么是定制转换器**：实测 60 篇随机样本的往返一致性，通用库 `markdownify` 中位
//! 0.915，定制规则 0.9597。根因是为知正文的形态 —— 正文是 `div` **平铺**（不是 `<p>`），
//! 终端输出靠 div 逐行 + `&nbsp;` 缩进；代码源码藏在隐藏 `<textarea>` 里，旁边还有一份
//! CodeMirror 渲染镜像。现成库会把多行 div 合并成一整段，也会把代码读成两份。
//!
//! **两条硬约束**（§20.5 的实测教训，实现时不得违反）：
//! 1. 代码块必须**整块**吞掉 —— `<div class="wiz-code-container">` 连同内部隐藏
//!    `<textarea>`（源码）与**紧随其后**的 `<wiz_code_mirror>`（渲染镜像）一起处理，
//!    只产出**一份**围栏。原型第一版只切到 `</wiz_code_mirror>`，导致代码内容在 md 里
//!    出现两遍、文件膨胀 3–4 倍。
//! 2. 走 **token 流逐节点产出**，不用"字符串占位符回填"。占位符相邻时会被后续处理
//!    当成同一个文本节点，两个相邻代码块会粘成一个（原型第一版出现过
//!    ``-----``````shell``）。
//!
//! 保真口径见 §20.7「干净优先」：只保留语义（标题 / 列表 / 表格 / 代码围栏含语言 /
//! 粗斜体 / 链接 / 图片 / 引用 / 分隔线）。颜色、字号、对齐、列宽一律丢弃。

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts};
use html5ever::{local_name, LocalName};
use std::cell::RefCell;
use std::rc::Rc;

/// 库内 md 包的**正文档名**（§20.3）：库内一篇笔记 = 1 个 zip，正文即此条目，
/// 随包附件保留原条目名（`index_files/...`）—— 故 md 里写 `![](index_files/xxx.png)`。
/// 全仓引用此常量，避免"哪里拼 `note.md`"各写一份。
pub const NOTE_MD: &str = "note.md";

/// 统计量：便于 CLI 输出与单测断言
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MdStats {
    /// 产出的代码围栏数（含由"代码排版表"降级来的围栏）
    pub code_blocks: usize,
    /// 其中由**代码排版表**（`lntable` / `crayon-table` / `hljs-ln` / `diff-table` 等
    /// 以表格布局承载"行号 + 代码"的写法）降级而来的围栏数。
    /// 这类表格**不是**数据表格，必须转成围栏，否则会渲染成一张带着行号列的怪表。
    pub code_tables: usize,
    /// 被吞掉的 CodeMirror 镜像块数
    pub mirrors: usize,
    /// 产出的 GFM 表格数
    pub tables: usize,
    /// 因是"块级布局表"（`d-block` 等单列包裹）而被**摊平**（不产表格）的数
    pub layout_tables: usize,
    /// 丢弃的内联 base64 图（如 CSDN 分享二维码）
    pub dropped_data_imgs: usize,
}

// ============================ 公开 API ============================

/// 为知原生 `index.html` → Markdown 正文。
pub fn html_to_md(html: &str) -> String {
    html_to_md_with_stats(html).0
}

/// 同上，附带统计量。
pub fn html_to_md_with_stats(html: &str) -> (String, MdStats) {
    let conv = Rc::new(RefCell::new(Conv::new()));
    let tok = Tokenizer::new(Sink(Rc::clone(&conv)), TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(StrTendril::from(html));
    let _ = tok.feed(&input);
    tok.end();
    drop(tok); // 释放唯一的外部引用，才能取回 Conv
    let mut conv = Rc::try_unwrap(conv)
        .ok()
        .expect("Tokenizer 已释放，引用应唯一")
        .into_inner();
    let stats = conv.stats.clone();
    (conv.finish(), stats)
}

/// Markdown → HTML 片段（不含 `<html>` 外壳）。
pub fn md_to_html(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let mut out = String::with_capacity(md.len() * 2 + 256);
    html::push_html(&mut out, Parser::new_ext(md, opts));
    out
}

/// Markdown → 完整 HTML 文档（供 `wiznote://` 阅读与编辑预览共用，§20.7）。
pub fn md_to_html_document(md: &str, title: &str) -> String {
    let body = md_to_html(md);
    format!(
        "<!DOCTYPE html>\n<html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>{title}</title><style>{css}</style></head><body class=\"md-body\">{body}</body></html>",
        title = crate::extract::html_escape(title),
        css = MD_CSS,
        body = body
    )
}

/// 阅读样式：与阅读区观感一致，跟随系统深浅色。
const MD_CSS: &str = r#"
:root { color-scheme: light dark; }
body.md-body {
  margin: 0; padding: 24px 32px 64px; max-width: 900px;
  font-family: -apple-system, BlinkMacSystemFont, "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
  font-size: 15px; line-height: 1.75; color: #24292f; background: #ffffff;
  word-wrap: break-word; overflow-wrap: break-word;
}
body.md-body h1, body.md-body h2, body.md-body h3,
body.md-body h4, body.md-body h5, body.md-body h6 {
  margin: 1.6em 0 .6em; line-height: 1.3; font-weight: 600;
}
body.md-body h1 { font-size: 1.7em; border-bottom: 1px solid #e6e8eb; padding-bottom: .3em; }
body.md-body h2 { font-size: 1.4em; border-bottom: 1px solid #e6e8eb; padding-bottom: .25em; }
body.md-body h3 { font-size: 1.2em; }
body.md-body p { margin: .8em 0; }
body.md-body a { color: #0969da; text-decoration: none; }
body.md-body a:hover { text-decoration: underline; }
body.md-body code {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: .9em; background: rgba(135,131,120,.15); padding: .15em .35em; border-radius: 3px;
}
body.md-body pre {
  background: #f6f8fa; border: 1px solid #e6e8eb; border-radius: 6px;
  padding: 12px 14px; overflow-x: auto; line-height: 1.5;
}
body.md-body pre code { background: none; padding: 0; font-size: .88em; }
body.md-body blockquote {
  margin: .8em 0; padding: .1em 1em; color: #57606a; border-left: 4px solid #d0d7de;
}
body.md-body table { border-collapse: collapse; margin: 1em 0; display: block; overflow-x: auto; }
body.md-body th, body.md-body td { border: 1px solid #d0d7de; padding: 6px 12px; }
body.md-body th { background: #f6f8fa; font-weight: 600; }
body.md-body img { max-width: 100%; height: auto; }
body.md-body hr { border: none; border-top: 1px solid #d0d7de; margin: 1.6em 0; }
body.md-body ul, body.md-body ol { padding-left: 1.8em; margin: .8em 0; }
body.md-body li { margin: .25em 0; }
@media (prefers-color-scheme: dark) {
  body.md-body { color: #e6edf3; background: #1c1c1e; }
  body.md-body h1, body.md-body h2 { border-bottom-color: #30363d; }
  body.md-body a { color: #6cb6ff; }
  body.md-body code { background: rgba(110,118,129,.4); }
  body.md-body pre { background: #161b22; border-color: #30363d; }
  body.md-body blockquote { color: #8b949e; border-left-color: #30363d; }
  body.md-body th, body.md-body td { border-color: #30363d; }
  body.md-body th { background: #161b22; }
  body.md-body hr { border-top-color: #30363d; }
}
"#;

// ============================ 输出缓冲 ============================

/// 拼接缓冲：负责"块间空行 / 行间单换行"的分隔，避免块粘连。
/// 用显式方法而不是让调用方拼 `\n`，是为了让"两个相邻块不会粘成一个"成为结构性保证
/// （§20.5 缺陷 ① 的根因就是拼接层没有分隔约束）。
#[derive(Default)]
struct Out {
    buf: String,
}

impl Out {
    /// 追加一个**块**：与已有内容之间恰好一个空行。
    fn block(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() {
            return;
        }
        self.trim_tail();
        if !self.buf.is_empty() {
            self.buf.push_str("\n\n");
        }
        self.buf.push_str(s);
    }

    /// 追加**一行**：与已有内容之间单换行（若已有空行则保留空行，不被压掉）。
    /// 用于列表项 —— 列表项属于同一个块。
    fn line(&mut self, s: &str) {
        let s = s.trim_end();
        if s.trim().is_empty() {
            return;
        }
        if !self.buf.is_empty() && !self.buf.ends_with('\n') {
            self.buf.push('\n');
        }
        self.buf.push_str(s);
    }

    /// 保证与之前的内容之间是空行（列表/表格等块级结构前调用）
    fn ensure_blank(&mut self) {
        if self.buf.trim().is_empty() {
            return;
        }
        self.trim_tail();
        self.buf.push_str("\n\n");
    }

    fn trim_tail(&mut self) {
        while matches!(self.buf.chars().last(), Some('\n') | Some(' ') | Some('\t') | Some('\r')) {
            self.buf.pop();
        }
    }
}

// ============================ 转换器 ============================

/// **转换器版本**（§20.3/M3）：标识"包内正文是怎么生成的"这一口径。
///
/// 任何会改变 [`html_to_md`] 输出的改动都**必须递增**它 —— 否则已有的 md 库在下次导出时
/// 会因"源没变、清单没变"被判定为可复用（`export.rs` 的增量复用第一级），
/// **修复永远落不进已建好的库**。导出侧把它写进清单 meta（`md_converter`）并参与复用判定。
///
/// v1 → v2：`dedent_layout_lines` —— 去掉行首排版缩进（详见该函数）。
/// v2 → v3：`collapse_inline_ws` —— 行内文本的换行/连续空白按 HTML 语义折叠成一个空格
/// （v2 的 dedent 只覆盖文本块，列表项里的排版行仍在 ⇒ 仍有 258 篇渲染成代码块）。
pub const CONVERTER_VERSION: &str = "3";

/// 归一化块内每一行的**行首排版缩进**。
///
/// **为什么必须做**：Markdown 会把"以 Tab 或 ≥4 空格开头、且位于块首（前一行为空行）"的行
/// 当成**缩进代码块**。为知正文用 `&nbsp;` 逐行排版，转换后就成了行首空白 —— 实测 md 库里
/// 1780 篇有 **307 篇**中招，渲染时凭空多出 **13115 个代码块**（正文被显示成代码）。
/// 这些空白在 HTML 里本来就看不见（连续空白折叠），故按 §20.7「干净优先」一律去掉。
///
/// **刻意不动**两类缩进 —— 它们有语法含义：
/// - 列表项（`    - x` / `1. x`）：嵌套层级靠缩进表达；
/// - 引用（`> x`）。
///
/// **围栏代码块完全不经这里**（`emit_code` 直出 `self.out`），故代码的缩进原样保留。
fn dedent_layout_lines(s: &str) -> String {
    s.lines()
        .map(|l| {
            let t = l.trim_start_matches([' ', '\t']);
            if t.is_empty() {
                // 只含空白的行按空行处理（它本就是"块界"信号，保留反而更容易触发代码块）
                return String::new();
            }
            let ws = &l[..l.len() - t.len()];
            let cols = ws
                .chars()
                .map(|c| if c == '\t' { 4 } else { 1 })
                .sum::<usize>();
            if cols >= 4 && !is_md_block_start(t) {
                t.to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 该行去掉缩进后是否是**有语法含义的块起点**（列表项 / 引用 / 标题 / 围栏 / 表格行）——
/// 是则其缩进必须保留（见 [`dedent_layout_lines`]）。
fn is_md_block_start(t: &str) -> bool {
    let mut ch = t.chars();
    match ch.next() {
        Some('>') => true,
        Some('#') => t.trim_start_matches('#').starts_with(' '),
        Some('`') => t.starts_with("```"),
        Some('|') => true, // 表格行
        Some('-') | Some('*') | Some('+') => {
            let rest = &t[1..];
            rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')
        }
        Some(c) if c.is_ascii_digit() => {
            let rest = t.trim_start_matches(|c: char| c.is_ascii_digit());
            rest.starts_with(". ") || rest.starts_with(") ")
        }
        _ => false,
    }
}

#[derive(Default)]
struct CodeState {
    mode: String,
    src: Option<String>,
    ta_buf: String,
    in_textarea: bool,
    div_depth: i32,
}

#[derive(Default)]
struct TableState {
    rows: Vec<Vec<String>>,
    cur_row: Vec<String>,
    cell: String,
    in_cell: bool,
    div_depth: i32,
    complex: bool,
    /// 代码排版表（`lntable` / `crayon-table` / `hljs-ln` / `diff-table` / `syntaxhighlighter`…）：
    /// 单元格按「行号槽 / 内容」甄别，最后产出**一个代码围栏**而不是数据表格。
    code: bool,
    /// 代码排版表的围栏语言（如 `diff`）
    lang: String,
    /// **嵌套深度**。为知的"布局表"里常再套一层表（例如版式表 → 代码表），
    /// 内层的 `</table>` 绝不能当成外层的收口，否则外层表被提前结束、单元格内容错位。
    depth: i32,
    /// 行的**原始**单元格（保留真换行、不做 md 转义、不转 `<br>`）。
    /// 只有代码排版表会用到它 —— 普通数据表格仍走 `rows` 的 `sanitize_cell` 通道。
    raw_rows: Vec<Vec<RawCell>>,
    raw_row: Vec<RawCell>,
    raw_cell: String,
    /// 当前单元格的 class 是否命中行号槽标记
    raw_cell_by_class: bool,
}

/// 代码排版表里的一个单元格（保留**真换行**、不做 md 转义）
#[derive(Default)]
struct RawCell {
    /// class 里带行号槽标记（`nums` / `number` / `gutter` / `blob-num` / `lineno`）
    by_class: bool,
    /// 内容为空或全是数字 —— 行号槽的典型形态
    by_text: bool,
    text: String,
}

struct ListCtx {
    ordered: bool,
    idx: usize,
}

struct Conv {
    out: Out,
    /// 当前块的缓冲（行内内容累积处）
    cur: String,
    /// 已开启元素栈（标签名）
    stack: Vec<LocalName>,
    /// 处于 `<head>` 内（用标志位而非深度：万一 `</head>` 缺失，`<body>` 也能收回来）
    in_head: bool,
    /// `style`/`script`/`wiz_code_mirror` 等**有结束标签**的忽略区深度
    ignore: usize,
    code: Option<CodeState>,
    table: Option<TableState>,
    lists: Vec<ListCtx>,
    /// 标记位置：用于"空标记回退"（`**` 内为空则不产出标记）
    marks: Vec<(usize, &'static str)>,
    /// `<a>` 的 href 与起始标记位置
    link: Option<(String, usize)>,
    /// `<pre>` 内容缓冲
    pre: Option<String>,
    /// 裸 `<pre>` 的围栏语言（取自 `class="language-x"` / `lang-x` / 外层 `highlight-x`）
    pre_lang: String,
    /// `<pre>` 内部正在跳过的"页面零件"深度（行号 `<ul class="pre-numbering">`、
    /// 复制按钮 `<div class="hljs-button">` 等 —— 它们是站点加在代码旁边的 DOM，
    /// 不是代码本身）。见 [`is_pre_chrome`]。
    pre_skip: usize,
    /// 当前标题级数
    heading: Option<u8>,
    /// 正在一个"布局表"里（见 [`is_layout_table`]）：`<table>`/`<tr>`/`<td>` 等结构标签
    /// 一律透明，内容按普通块级元素处理
    in_layout_table: bool,
    /// 布局表的嵌套深度（正常情况下恒为 1）
    layout_depth: i32,
    stats: MdStats,
}

impl Conv {
    fn new() -> Self {
        Conv {
            out: Out::default(),
            cur: String::with_capacity(1024),
            stack: Vec::with_capacity(16),
            in_head: false,
            ignore: 0,
            code: None,
            table: None,
            lists: Vec::new(),
            marks: Vec::new(),
            link: None,
            pre: None,
            pre_lang: String::new(),
            pre_skip: 0,
            heading: None,
            in_layout_table: false,
            layout_depth: 0,
            stats: MdStats::default(),
        }
    }

    fn finish(&mut self) -> String {
        self.flush_block();
        // 块间至多一个空行；首尾不留空白
        let re = regex::Regex::new(r"\n{3,}").expect("regex");
        let s = re.replace_all(&self.out.buf, "\n\n");
        s.trim().to_string()
    }

    // ---------- 缓冲输出 ----------

    fn flush_block(&mut self) {
        let mut t = dedent_layout_lines(self.cur.trim());
        self.cur.clear();
        self.marks.clear();
        if t.is_empty() {
            return;
        }
        if self.in_blockquote() {
            t = t
                .lines()
                .map(|l| {
                    if l.trim().is_empty() {
                        ">".to_string()
                    } else {
                        format!("> {}", l)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
        self.out.block(&t);
    }

    fn flush_line(&mut self) {
        let t = self.cur.trim_end().to_string();
        self.cur.clear();
        self.marks.clear();
        if !t.trim().is_empty() {
            self.out.line(&t);
        }
    }

    fn emit_code(&mut self, lang: &str, src: &str) {
        let body = src.trim_matches('\n');
        if body.trim().is_empty() {
            // 空围栏没有意义（§20.5 缺陷 ②）
            return;
        }
        self.flush_block();
        self.out.ensure_blank();
        let fence = format!("```{}\n{}\n```", lang.trim().to_lowercase(), body);
        self.out.block(&fence);
        self.stats.code_blocks += 1;
    }

    fn emit_table(&mut self, rows: &[Vec<String>], complex: bool) {
        self.flush_block();
        if rows.is_empty() {
            return;
        }
        if complex {
            // 合并单元格等 GFM 表达不了的结构：降级为"竖线分隔的纯文本行"，
            // 至少保住内容与行序（比丢掉或产错表格安全）。
            let lines: Vec<String> = rows
                .iter()
                .map(|r| r.join(" | ").trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            self.out.block(&lines.join("\n"));
            self.stats.tables += 1;
            return;
        }
        let ncol = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if ncol == 0 {
            return;
        }
        let pad = |r: &Vec<String>| -> Vec<String> {
            let mut v = r.clone();
            v.resize(ncol, String::new());
            v
        };
        let mut out = String::new();
        let head = pad(&rows[0]);
        out.push_str("| ");
        out.push_str(&head.join(" | "));
        out.push_str(" |\n|");
        for _ in 0..ncol {
            out.push_str(" --- |");
        }
        for r in &rows[1..] {
            let r = pad(r);
            out.push_str("\n| ");
            out.push_str(&r.join(" | "));
            out.push_str(" |");
        }
        self.out.block(&out);
        self.stats.tables += 1;
    }

    /// 代码排版表 → **一个**代码围栏。
    ///
    /// 这类"表"（`lntable` / `crayon-table` / `hljs-ln` / `diff-table` / `syntaxhighlighter`）
    /// 是站点为了摆"行号槽 + 代码"而用的表格布局，**不是数据表格** —— 照数据表格渲染会得到
    /// 一张带行号列的怪表，且代码会被 md 转义（`[mons]` → `\[mons\]`）。
    fn emit_code_table(&mut self, tb: &TableState) {
        let mut lines: Vec<String> = Vec::new();
        for row in &tb.raw_rows {
            // 本行只要有单元格带行号槽 class，就**以 class 为准** —— 这样代码里的空行才保得住；
            // 没有 class 信号时退回内容判定（空 / 全数字 → 行号槽）。
            let use_class = row.iter().any(|c| c.by_class);
            for c in row {
                let gutter = if use_class { c.by_class } else { c.by_text };
                if gutter {
                    continue;
                }
                // class 信号可信时，空的**内容**格要当成代码里的空行保留；
                // 只靠内容判定时，空格无从区分，只能丢掉。
                if use_class || !c.text.trim().is_empty() {
                    lines.push(c.text.clone());
                }
            }
        }
        let body = lines.join("\n");
        if body.trim().is_empty() {
            return;
        }
        self.emit_code(&tb.lang, &body);
        self.stats.code_tables += 1;
    }

    /// 追加文本到当前块（做 md 转义与空白归一）
    fn push_text(&mut self, s: &str) {
        let s = normalize_ws(s);
        if s.is_empty() {
            return;
        }
        // 上一段正文以**裸 `<`**收尾（html5ever 把 `&lt;` 单独切成了一个 token），
        // 而本段紧接着是字母 / `/` / `!` / `?` ⇒ 两段拼起来才看得出是标签形态。
        // 在这里补转义符：`&lt;property&gt;` → `\<property>`，而 `a &lt; b` 保持干净
        // （下一字符是空格，不命中）。不做这步时 `<property>` / `<?xml ...?>` 会原样漏出去，
        // 被 CommonMark 当原始 HTML 收下、渲染时整段消失（实测 12 篇笔记正文丢失）。
        fixup_lone_lt(&mut self.cur, &s);
        let s = escape_md_text(&s);
        // 块首不留空格（但已有内联标记时不能整段清掉）
        if self.cur.trim().is_empty() && self.marks.is_empty() {
            self.cur.clear();
            self.cur.push_str(s.trim_start());
        } else {
            // **HTML 语义**：文本节点里的换行 / 连续空白只是排版，渲染成一个空格。
            // 原样带进 md 会留下"行首 4+ 空格的排版行"，叠加列表标记后被 CommonMark
            // 判成**缩进代码块**（实测 md 库 258 篇中招、凭空多出 8150 个代码块）。
            // 真换行由 `<br>`（[`Conv::on_tag`]）与块级标签负责，不靠文本节点里的 `\n`。
            let c = collapse_inline_ws(&s);
            let c = if self.cur.ends_with([' ', '\n']) { c.trim_start_matches(' ') } else { c.as_str() };
            self.cur.push_str(c);
        }
    }

    fn in_li(&self) -> bool {
        self.stack.iter().any(|n| *n == local_name!("li"))
    }

    fn in_blockquote(&self) -> bool {
        self.stack.iter().any(|n| *n == local_name!("blockquote"))
    }

    // ---------- token 处理 ----------

    fn on_text(&mut self, t: &str) {
        if self.ignore > 0 || self.in_head {
            return;
        }
        if let Some(cd) = self.code.as_mut() {
            if cd.in_textarea {
                cd.ta_buf.push_str(t);
            }
            return;
        }
        if self.pre.is_some() {
            // 跳过区（行号槽 / 复制按钮）里的文本同样不能收
            if self.pre_skip == 0 {
                if let Some(p) = self.pre.as_mut() {
                    p.push_str(t);
                }
            }
            return;
        }
        if let Some(tb) = self.table.as_mut() {
            if tb.in_cell {
                let s = normalize_ws(t);
                if tb.code {
                    // 代码排版表：**绝不能**做 md 转义，否则代码里的 `[mons]` 会变成 `\[mons\]`
                    tb.raw_cell.push_str(&s);
                } else {
                    // 表格单元格和正文一样要补"裸 `<`"的转义 —— 漏掉时单元格里的
                    // `<?xml version=...?>` 会被当原始 HTML 丢掉（实测整段 XML 渲染后消失）
                    fixup_lone_lt(&mut tb.cell, &s);
                    tb.cell.push_str(&escape_md_text(&s));
                }
            }
            return;
        }
        self.push_text(t);
    }

    fn on_tag(&mut self, tag: &Tag) {
        let start = tag.kind == TagKind::StartTag;
        let n = tag.name.clone();

        // `<head>` 用标志位管理：`<body>` 一出现就结束（HTML5 里 head 的结束是隐式的）
        if n == local_name!("head") {
            self.in_head = start;
            return;
        }
        if start && n == local_name!("body") {
            self.in_head = false;
        }
        if self.in_head {
            return;
        }

        // void 元素（无结束标签）单标签忽略 —— **绝不能**计入 ignore 深度，
        // 否则深度永远减不回去、元数据之后的整篇正文都会被吞掉
        if is_void_ignored(&n) {
            return;
        }

        // ---- 布局表（`d-block` 等单列包裹）：结构标签透明，内容按普通块级元素处理 ----
        //
        // 只吞**表格结构标签**，别的一概放行 —— 否则 `</table>` 会去 pop 外层元素的栈，
        // 把 `<div>` 之类的配对关系搞乱。
        if self.in_layout_table && is_table_struct(&n) {
            if n == local_name!("table") {
                if start {
                    self.layout_depth += 1;
                } else {
                    self.layout_depth -= 1;
                    if self.layout_depth <= 0 {
                        self.in_layout_table = false;
                    }
                }
            }
            return;
        }

        if self.ignore > 0 {
            if is_ignored_container(&n) {
                if start {
                    self.ignore += 1;
                } else {
                    self.ignore -= 1;
                }
            }
            return;
        }
        if is_ignored_container(&n) {
            if start {
                self.ignore = 1;
                if n.as_ref() == WIZ_MIRROR {
                    // 被吞掉的渲染镜像：说明本篇有带渲染态的代码块
                    self.stats.mirrors += 1;
                }
            }
            return;
        }

        // ---- 代码块容器：整块吞掉（源码 textarea + 渲染镜像一起） ----
        if self.code.is_some() {
            if start {
                if n == local_name!("textarea") {
                    if let Some(cd) = self.code.as_mut() {
                        cd.in_textarea = true;
                        cd.ta_buf.clear();
                    }
                } else if n == local_name!("div") {
                    if let Some(cd) = self.code.as_mut() {
                        cd.div_depth += 1;
                    }
                }
            } else if n == local_name!("textarea") {
                if let Some(cd) = self.code.as_mut() {
                    cd.in_textarea = false;
                    let raw = std::mem::take(&mut cd.ta_buf);
                    if !raw.trim().is_empty() {
                        cd.src = Some(crate::extract::decode_entities(&raw));
                    }
                }
            } else if n == local_name!("div") {
                let done = {
                    let cd = self.code.as_mut().expect("code");
                    cd.div_depth -= 1;
                    cd.div_depth <= 0
                };
                if done {
                    let cd = self.code.take().expect("code");
                    if let Some(src) = cd.src {
                        self.emit_code(&cd.mode, &src);
                    }
                    // 紧随其后的 `<wiz_code_mirror>` 由 is_ignored 统一忽略；
                    // 漏掉它会让渲染镜像作为正文留下、代码内容重复一遍
                }
            }
            return;
        }

        // ---- 表格内部 ----
        if self.table.is_some() {
            if !start && n == local_name!("table") {
                if self.table.as_ref().expect("table").depth > 0 {
                    // 内层表的收口：只出栈，不结束外层（见 `TableState::depth`）
                    self.table.as_mut().expect("table").depth -= 1;
                    return;
                }
                // 表格结束：必须在这里收口，否则 `</table>` 会被 on_table_tag 静默吞掉
                let mut tb = self.table.take().expect("table");
                if tb.in_cell {
                    // 畸形 HTML 常缺 `</td>`：若不在收口前把单元格收进来，整格内容就丢了
                    if tb.code {
                        if !tb.raw_cell.trim().is_empty() {
                            let text = trim_blank_edges(&tb.raw_cell);
                            tb.raw_row.push(RawCell {
                                by_class: tb.raw_cell_by_class,
                                by_text: is_line_number(&text),
                                text,
                            });
                        }
                    } else if !tb.cell.trim().is_empty() {
                        let cell = sanitize_cell(&tb.cell);
                        tb.cur_row.push(cell);
                    }
                    tb.in_cell = false;
                }
                if !tb.raw_row.is_empty() {
                    let r = std::mem::take(&mut tb.raw_row);
                    tb.raw_rows.push(r);
                }
                if tb.code {
                    self.emit_code_table(&tb);
                } else {
                    let mut rows = tb.rows;
                    if !tb.cur_row.is_empty() {
                        rows.push(tb.cur_row);
                    }
                    self.emit_table(&rows, tb.complex);
                }
                return;
            }
            self.on_table_tag(tag, start, &n);
            return;
        }

        // ---- 代码围栏（非为知容器的裸 `<pre>`） ----
        // ⚠️ 这里必须**自己**处理 `</pre>`：早退是为了不让 `<pre>` 里的标签产出内联标记，
        // 但若把结束标签也一并早退掉，`self.pre` 就永远是 `Some` —— 该 `<pre>` 之后
        // （含它自己）的整篇正文都会被吞掉。实测 1780 篇里有 20 篇因此产出 0 字节。
        if self.pre.is_some() {
            if !start && n == local_name!("pre") {
                self.pre_skip = 0;
                if let Some(p) = self.pre.take() {
                    let lang = std::mem::take(&mut self.pre_lang);
                    let p = strip_trailing_line_numbers(&p);
                    self.emit_code(&lang, &p);
                }
            } else if self.pre_skip > 0 {
                // 正在跳过的页面零件内部：只维护深度
                if start {
                    self.pre_skip += 1;
                } else {
                    self.pre_skip -= 1;
                }
            } else if start && is_pre_chrome(tag) {
                // 复制 / AI按钮：整块跳过
                self.pre_skip = 1;
            } else if start && self.pre_lang.is_empty() && n == local_name!("code") {
                // 语言常挂在**内层 `<code>`** 上（CSDN/Prism：`<pre class="prettyprint">`
                // 里套 `<code class="prism language-shell has-numbering">`），`<pre>` 自己
                // 反而没有 —— 这里补一次探测
                self.pre_lang = lang_of_class(&attr_of(tag, "class"));
            } else if start && (n == local_name!("br") || n == local_name!("li")) {
                // `<br>` 与 `<li>` 都当换行：CSDN 有个变体把**代码本体**写成
                // `<pre><code><ol><li>一行代码</li>…</ol>`，不把 `<li>` 当换行会粘成一整行
                if let Some(p) = self.pre.as_mut() {
                    p.push('\n');
                }
            }
            return;
        }

        if start {
            match n.as_ref() {
                "div" => {
                    let class = attr_of(tag, "class");
                    if class.contains(CODE_CONTAINER) {
                        self.code = Some(CodeState {
                            mode: attr_of(tag, "data-mode"),
                            div_depth: 1,
                            ..Default::default()
                        });
                        return;
                    }
                    self.stack.push(n);
                }
                "table" => {
                    let cls = attr_of(tag, "class");
                    if is_layout_table(&cls) {
                        // 单列包裹表（GitHub 评论正文等）：内容其实是普通块级文本，
                        // 照数据表格渲染会变成一张单元格里塞满段落的怪表 ⇒ 摊平
                        self.in_layout_table = true;
                        self.layout_depth = 1;
                        self.stats.layout_tables += 1;
                        return;
                    }
                    let code = is_code_table(&cls);
                    self.table = Some(TableState {
                        div_depth: 0,
                        code,
                        lang: if code { code_table_lang(&cls) } else { String::new() },
                        ..Default::default()
                    });
                }
                "pre" => {
                    self.flush_block();
                    // 裸 `<pre>` 的语言只认自身 class 上的显式前缀（`language-x` / `lang-x`
                    // / `highlight-x`）；认不出就产无语言围栏 —— 内容不丢是第一位的
                    self.pre_lang = lang_of_class(&attr_of(tag, "class"));
                    self.pre = Some(String::new());
                }
                "ul" | "ol" => {
                    self.flush_block();
                    self.out.ensure_blank();
                    self.lists.push(ListCtx {
                        ordered: n == local_name!("ol"),
                        idx: 0,
                    });
                    self.stack.push(n);
                }
                "li" => {
                    let depth = self.lists.len().saturating_sub(1);
                    let marker = match self.lists.last_mut() {
                        Some(ctx) => {
                            ctx.idx += 1;
                            if ctx.ordered {
                                format!("{}. ", ctx.idx)
                            } else {
                                "- ".to_string()
                            }
                        }
                        None => "- ".to_string(),
                    };
                    if !self.cur.trim().is_empty() {
                        self.flush_line();
                    }
                    // 嵌套只表达"**一层**嵌套"（2 空格），再深也压回 2 —— Markdown 里
                    // 行首 ≥4 空格会被读成**缩进代码块**，而源 HTML 的上层列表项常常是
                    // 空壳或夹着段落（网页导航抓手），链一断，深层缩进就整片变成代码
                    // （实测 4 空格/层 ⇒ 258 篇中招；2 空格则永远不会命中 4 空格红线）。
                    // 2 空格也正是"父项内容缩进"，故一层嵌套是**正确**的 Markdown 写法。
                    if depth > 0 {
                        self.cur.push_str("  ");
                    }
                    self.cur.push_str(&marker);
                    self.stack.push(n);
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    self.flush_block();
                    let lvl = n.as_ref().as_bytes()[1] - b'0';
                    self.cur.push_str(&"#".repeat(lvl as usize));
                    self.cur.push(' ');
                    self.heading = Some(lvl);
                    self.stack.push(n);
                }
                "hr" => {
                    self.flush_block();
                    self.out.block("---");
                }
                "blockquote" => {
                    self.flush_block();
                    self.stack.push(n);
                }
                "br" => {
                    if self.pre.is_none() {
                        self.cur.push('\n');
                    }
                }
                "img" => {
                    let src = attr_of(tag, "src");
                    if src.starts_with("data:") {
                        // 页面杂物（如 CSDN 分享二维码的 base64 内联图）
                        self.stats.dropped_data_imgs += 1;
                        return;
                    }
                    let alt = attr_of(tag, "alt");
                    let src = normalize_ws(&src);
                    if src.trim().is_empty() {
                        return;
                    }
                    self.cur.push_str(&format!("![{}]({})", alt.trim(), src.trim()));
                }
                "a" => {
                    let href = normalize_ws(&attr_of(tag, "href"));
                    self.cur.push('[');
                    self.link = Some((href, self.cur.len()));
                }
                "strong" | "b" => self.push_marker("**"),
                "em" | "i" => self.push_marker("*"),
                "code" => self.push_marker("`"),
                "p" | "section" | "article" | "figure" | "figcaption" | "dd" | "dt" | "main"
                | "header" | "footer" | "aside" | "form" => {
                    self.stack.push(n);
                }
                _ => {
                    self.stack.push(n);
                }
            }
        } else {
            // 结束标签
            match n.as_ref() {
                "div" => {
                    self.stack.pop();
                    if self.in_li() {
                        // 列表项内的 div 换行即可，不能另起块（会打断列表）
                        self.cur.push('\n');
                    } else {
                        self.flush_block();
                    }
                }
                "pre" => {
                    // 正常路径不可达：`self.pre.is_some()` 时入口就早退、`</pre>` 由那段收口。
                    // 只有畸形 HTML（多出 `</pre>`）会走到这里，退化成普通标签即可。
                    self.stack.pop();
                }
                "li" => {
                    self.stack.pop();
                    self.flush_line();
                }
                "ul" | "ol" => {
                    self.stack.pop();
                    self.lists.pop();
                    self.flush_line();
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    self.stack.pop();
                    self.heading = None;
                    self.flush_block();
                }
                "blockquote" => {
                    self.stack.pop();
                    self.flush_block();
                }
                "a" => {
                    if let Some((href, mark)) = self.link.take() {
                        let empty = self.cur.len() <= mark;
                        if empty {
                            self.cur.push_str(&href);
                        }
                        self.cur.push_str("](");
                        self.cur.push_str(&href);
                        self.cur.push(')');
                    }
                }
                "strong" | "b" => self.close_marker("**"),
                "em" | "i" => self.close_marker("*"),
                "code" => self.close_marker("`"),
                "p" | "section" | "article" | "figure" | "figcaption" | "dd" | "dt" | "main"
                | "header" | "footer" | "aside" | "form" | "span" | "font" | "u" | "sub"
                | "sup" | "s" | "del" | "strike" | "tbody" | "thead" | "tr" => {
                    self.stack.pop();
                    if matches!(n.as_ref(), "p" | "dd" | "dt" | "figcaption") {
                        if self.in_li() {
                            self.cur.push('\n');
                        } else {
                            self.flush_block();
                        }
                    }
                }
                _ => {
                    self.stack.pop();
                }
            }
        }
    }

    fn push_marker(&mut self, m: &'static str) {
        let pos = self.cur.len();
        self.cur.push_str(m);
        self.marks.push((pos, m));
    }

    fn close_marker(&mut self, m: &'static str) {
        match self.marks.pop() {
            Some((pos, mm)) if mm == m => {
                if self.cur.len() == pos + m.len() {
                    // 标记内为空 → 回退，不产出空标记
                    self.cur.truncate(pos);
                } else {
                    self.cur.push_str(m);
                }
            }
            _ => self.cur.push_str(m),
        }
    }

    fn on_table_tag(&mut self, tag: &Tag, start: bool, n: &LocalName) {
        let tb = self.table.as_mut().expect("table");

        // 内层表（版式表里再套一层代码表之类）：结构标签一律**透明**，
        // 文本照旧并入外层当前单元格。不这么做时内层的 `</td>` 会把外层的
        // `in_cell` 关掉，后续正文就没人接管了（实测整段答案正文丢失）。
        if tb.depth > 0 {
            if start {
                if *n == local_name!("table") {
                    tb.depth += 1;
                } else if tb.in_cell
                    && matches!(n.as_ref(), "tr" | "td" | "th" | "div" | "br")
                {
                    tb.cell.push('\n');
                    tb.raw_cell.push('\n');
                }
            }
            return;
        }

        if start {
            match n.as_ref() {
                "table" => tb.depth += 1,
                "tr" => {
                    if !tb.cur_row.is_empty() {
                        let r = std::mem::take(&mut tb.cur_row);
                        tb.rows.push(r);
                    }
                    if !tb.raw_row.is_empty() {
                        let r = std::mem::take(&mut tb.raw_row);
                        tb.raw_rows.push(r);
                    }
                }
                "td" | "th" => {
                    tb.in_cell = true;
                    tb.cell.clear();
                    tb.raw_cell.clear();
                    tb.raw_cell_by_class = false;
                    if tb.code {
                        tb.raw_cell_by_class = gutter_class(&attr_of(tag, "class"));
                    }
                    // 合并单元格 GFM 表达不了：降级为竖线分隔的纯文本行
                    if bigger_than_one(&attr_of(tag, "colspan"))
                        || bigger_than_one(&attr_of(tag, "rowspan"))
                    {
                        tb.complex = true;
                    }
                }
                "div" => {
                    tb.div_depth += 1;
                    if tb.in_cell {
                        tb.cell.push('\n');
                        tb.raw_cell.push('\n');
                    }
                }
                "br" => {
                    if tb.in_cell {
                        tb.cell.push('\n');
                        tb.raw_cell.push('\n');
                    }
                }
                _ => {}
            }
        } else {
            match n.as_ref() {
                "td" | "th" => {
                    if tb.in_cell {
                        if tb.code {
                            let text = trim_blank_edges(&tb.raw_cell);
                            tb.raw_row.push(RawCell {
                                by_class: tb.raw_cell_by_class,
                                by_text: is_line_number(&text),
                                text,
                            });
                        } else {
                            let cell = sanitize_cell(&tb.cell);
                            tb.cur_row.push(cell);
                        }
                        tb.cell.clear();
                        tb.raw_cell.clear();
                        tb.in_cell = false;
                    }
                }
                "tr" => {
                    if !tb.cur_row.is_empty() {
                        let r = std::mem::take(&mut tb.cur_row);
                        tb.rows.push(r);
                    }
                    if !tb.raw_row.is_empty() {
                        let r = std::mem::take(&mut tb.raw_row);
                        tb.raw_rows.push(r);
                    }
                }
                "div" => tb.div_depth -= 1,
                _ => {}
            }
        }
    }
}

/// 单元格：换行转 `<br>`、竖线转义、去首尾空白
fn sanitize_cell(s: &str) -> String {
    let t: Vec<&str> = s.split('\n').map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
    let joined = t.join("<br>");
    if joined.trim().is_empty() {
        " ".to_string()
    } else {
        joined.replace('|', "\\|")
    }
}

/// 代码文本规整：去 `\r`、去行尾空白、去首尾空行，**保留行内换行**
fn trim_blank_edges(s: &str) -> String {
    let s = s.replace('\r', "");
    let lines: Vec<&str> = s.split('\n').map(|l| l.trim_end()).collect();
    let (mut a, mut b) = (0usize, lines.len());
    while a < b && lines[a].trim().is_empty() {
        a += 1;
    }
    while b > a && lines[b - 1].trim().is_empty() {
        b -= 1;
    }
    lines[a..b].join("\n")
}

/// 内容层面的行号槽判定：空、或全是数字/空白
fn is_line_number(s: &str) -> bool {
    s.trim().chars().all(|c| c.is_ascii_digit() || c.is_whitespace())
}

/// class 层面的行号槽标记。**故意不含 `lnt`** —— `lntable` 的两个单元格都是
/// `class="lntd"`，按 class 判会把代码格也误判成行号槽；那种表只能靠内容判定。
fn gutter_class(cls: &str) -> bool {
    const MARKS: &[&str] = &["nums", "number", "gutter", "blob-num", "lineno"];
    let c = cls.to_ascii_lowercase();
    MARKS.iter().any(|m| c.contains(m))
}

/// 属性值是否表示"跨 >1 格"（`colspan="2"`）
fn bigger_than_one(v: &str) -> bool {
    v.trim().parse::<u32>().map(|n| n > 1).unwrap_or(false)
}

/// 表格结构标签（布局表模式下要一律吞掉的那批）
fn is_table_struct(n: &LocalName) -> bool {
    matches!(
        n.as_ref(),
        "table" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th" | "caption" | "colgroup" | "col"
    )
}

/// 代码排版表的 class 信号：都是"用表格承载『行号 + 代码』"的已知写法
const CODE_TABLE_MARKS: &[&str] = &[
    "lntable",          // Hugo / Chroma
    "crayon-table",     // Crayon Syntax Highlighter (WordPress)
    "hljs-ln",          // highlight.js line numbers
    "syntaxhighlighter", // Alex Gorbatchev's SyntaxHighlighter
    "highlighter",      // 博客园等
    "diff-table",       // GitHub 统一 diff 视图
    "blob-code",        // 同上
    "crayon-code",
    "code-table",
    "chroma",
];

fn is_code_table(cls: &str) -> bool {
    let c = cls.to_ascii_lowercase();
    CODE_TABLE_MARKS.iter().any(|m| c.contains(m))
        || c.split_whitespace().any(|t| t == "diff")
}

/// 代码排版表的围栏语言：`diff-table` → `diff`；`syntaxhighlighter java` → `java`
fn code_table_lang(cls: &str) -> String {
    let toks: Vec<String> = cls.split_whitespace().map(|s| s.to_ascii_lowercase()).collect();
    if toks.iter().any(|t| t == "diff-table" || t == "diff") {
        return "diff".to_string();
    }
    if let Some(i) = toks.iter().position(|t| t == "syntaxhighlighter") {
        if let Some(l) = toks.get(i + 1).filter(|l| !l.is_empty()) {
            return l.clone();
        }
    }
    String::new()
}

/// 布局表：class 带 `d-block` 的 `<table>` 是**单列包裹**，内容其实是普通块级文本。
///
/// 实测依据（为知 1780 篇样本）：`d-block` 表共 77 个，**全部** `th=0` 且每行恰好 1 个
/// `td`（GitHub 评论正文的 `display:block` 包裹），没有一个是数据表格。
fn is_layout_table(cls: &str) -> bool {
    cls.split_whitespace().any(|t| t == "d-block")
}

/// 从 class 里推代码语言。只认**显式前缀**（`language-x` / `lang-x` / `highlight-x` /
/// `source-x` / `brush: x`），且要求剥完前缀后是个短标识 —— 否则 `wiz-editor-body`
/// 之类会被误当语言。认不出就返回空串（产无语言围栏）。
fn lang_of_class(cls: &str) -> String {
    for tok in cls.split_whitespace() {
        let t = tok.to_ascii_lowercase();
        let rest = t
            .strip_prefix("brush:")
            .or_else(|| t.strip_prefix("language-"))
            .or_else(|| t.strip_prefix("lang-"))
            .or_else(|| t.strip_prefix("highlight-"))
            .or_else(|| t.strip_prefix("source-"));
        let Some(rest) = rest else { continue };
        // `highlight-source-shell` 这类双前缀再剥一层
        let rest = rest
            .strip_prefix("source-")
            .or_else(|| rest.strip_prefix("highlight-"))
            .unwrap_or(rest);
        if !rest.is_empty()
            && rest.len() <= 16
            && rest.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '#')
        {
            return rest.to_string();
        }
    }
    String::new()
}

// ============================ TokenSink ============================

/// Tokenizer 会 move 走 sink，故用 `Rc` 让外部保留句柄以便取回转换结果。
struct Sink(Rc<RefCell<Conv>>);

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut c = self.0.borrow_mut();
        match token {
            Token::TagToken(tag) => {
                // 自闭合标签（`<br/>`、`<img/>`）只按开始标签处理
                let self_closing = tag.self_closing;
                let kind = tag.kind;
                c.on_tag(&tag);
                if self_closing && kind == TagKind::StartTag {
                    match tag.name.as_ref() {
                        "br" | "img" | "hr" | "meta" | "link" | "input" => {}
                        _ => c.on_tag(&Tag {
                            kind: TagKind::EndTag,
                            name: tag.name.clone(),
                            self_closing: true,
                            attrs: Vec::new(),
                        }),
                    }
                }
            }
            Token::CharacterTokens(t) => c.on_text(&t),
            Token::NullCharacterToken => {}
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

// ============================ 工具 ============================

const CODE_CONTAINER: &str = "wiz-code-container";
/// CodeMirror 渲染镜像的自定义标签。**不在** web_atoms 的静态 atom 表里，
/// 故只能按字符串比较（`local_name!("wiz_code_mirror")` 编译不过）。
const WIZ_MIRROR: &str = "wiz_code_mirror";

/// 忽略区容器：**都有结束标签**，可以安全地做深度计数。
///
/// ⚠️ 不要把 `head` / `meta` / `link` / `base` / `embed` 放进来：
///   - `head` 的结束在 HTML5 里可能是隐式的（见 `in_head` 标志）；
///   - `meta` / `link` / `base` / `embed` 是 **void 元素**，根本没有结束标签，
///     一旦计入深度就再也减不回去 —— 实测这会让 45.7% 的笔记（814/1780）产出 0 字节。
fn is_ignored_container(n: &LocalName) -> bool {
    *n == local_name!("style")
        || *n == local_name!("script")
        || *n == local_name!("title")
        || *n == local_name!("noscript")
        || *n == local_name!("iframe")
        || *n == local_name!("svg")
        || *n == local_name!("canvas")
        || *n == local_name!("object")
        || *n == local_name!("template")
        || n.as_ref() == WIZ_MIRROR
}

/// void 元素（HTML 规范里没有结束标签）：只丢弃标签自身，不改变忽略状态
fn is_void_ignored(n: &LocalName) -> bool {
    *n == local_name!("meta")
        || *n == local_name!("link")
        || *n == local_name!("base")
        || *n == local_name!("embed")
        || *n == local_name!("input")
        || *n == local_name!("param")
        || *n == local_name!("source")
        || *n == local_name!("track")
        || *n == local_name!("col")
        || *n == local_name!("area")
        || *n == local_name!("wbr")
}

/// 读属性值（不存在则空串）
fn attr_of(tag: &Tag, name: &str) -> String {
    tag.attrs
        .iter()
        .find(|a| a.name.local.as_ref() == name)
        .map(|a| a.value.to_string())
        .unwrap_or_default()
}

/// 上一段文本以**裸 `<`** 收尾时补上转义符。
///
/// html5ever 会把 `&lt;` 单独切成一个字符 token：那时看不到后文，无法判断它是否
/// 属于标签形态。等下一段正文到达、拼起来才看得出 ⇒ 在这里回填 `\`。
/// `&lt;property&gt;` → `\<property>`；`a &lt; b` 因下一字符是空格而不命中。
fn fixup_lone_lt(buf: &mut String, next: &str) {
    if !buf.ends_with('<') || buf.ends_with("\\<") {
        return;
    }
    match next.chars().next() {
        Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!' || c == '?' => {
            buf.pop();
            buf.push_str("\\<");
        }
        _ => {}
    }
}

/// `<pre>` 内部需要整块跳过的**页面零件**（不是代码）。
///
/// ⚠️ **故意不含** `line-numbers` / `has-numbering` / `linenums`：Prism、highlight.js 等
/// 会把这些 class 打在**代码本体**（`<pre class="line-numbers language-js">` 或
/// `<code class="prism language-shell has-numbering">`）上，实测有 70 篇如此 ——
/// 一旦把它们当"零件"跳过，整个代码块就没了。
fn is_pre_chrome(tag: &Tag) -> bool {
    // `<button>` 是交互控件，不可能出现在"逐字渲染"的代码里 —— 只能是站点的
    // 复制 / AI写代码按钮。**但 `ul`/`ol` 不能这么判**：实测有 CSDN 变体把代码本体
    // 放在 `<pre><code><ol><li>一行</li>…</ol>` 里，按结构跳过会把整段代码删掉。
    // 行号列表改用"`<li>` 当换行 + 末尾纯数字行整段剪掉"来识别（见 `strip_trailing_line_numbers`）。
    if tag.name.as_ref() == "button" {
        return true;
    }
    const MARKS: &[&str] = &[
        "pre-numbering", // CSDN：<ul class="pre-numbering"><li>1</li>…</ul>
        "hljs-button",   // CSDN：复制按钮 / AI写代码按钮
        "hljs-ln-numbers",
        "crayon-nums",
        "copy-code",
        "code-copy",
        "copy-button",
        "btn-copy",
    ];
    let cls = attr_of(tag, "class").to_ascii_lowercase();
    MARKS.iter().any(|m| cls.contains(m))
}

/// 剪掉围栏**末尾连续的纯数字行**（行号槽）。
///
/// CSDN 等站点把行号槽塞在 `<pre>` 内部末尾，且那个 `<ul>`/`<ol>` **常常不带 class**
/// （只有 `style`），单靠 class 认不出来。`<li>` 已按换行处理 ⇒ 行号会变成末尾一串
/// 独立的 `1` `2` `3`…，在这里整段剪掉。
///
/// 要求**连续 ≥2 行**才剪：代码最后一行的内容恰好是个数字时不至于被误伤。
fn strip_trailing_line_numbers(s: &str) -> String {
    let mut lines: Vec<&str> = s.split('\n').collect();
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let mut n = 0usize;
    while lines
        .last()
        .map(|l| {
            let t = l.trim();
            !t.is_empty() && t.chars().all(|c| c.is_ascii_digit())
        })
        .unwrap_or(false)
    {
        lines.pop();
        n += 1;
    }
    if n >= 2 {
        lines.join("\n")
    } else {
        s.to_string()
    }
}

/// 空白归一：零宽字符剔除、`&nbsp;` 转普通空格、`\r` 剔除
fn normalize_ws(s: &str) -> String {
    s.replace('\u{200b}', "")
        .replace('\u{a0}', " ")
        .replace('\r', "")
}

/// **行内文本的空白折叠**（HTML 语义）：连续空白（含换行）折叠成**一个空格**，
/// 段首/段尾的空白也各折叠成一个空格（是否真的留下由调用方按上下文决定）。
///
/// 为什么必须折叠：源 HTML 里为了排版缩进换行的文本节点，会把整行缩进原样带进 md，
/// 形成"行首 4+ 空格"的排版行；一旦落在列表项 / 段落续行里，CommonMark 就把它读成
/// **缩进代码块**（实测 md 库 258 篇中招、渲染时凭空多出 8150 个代码块）。
/// 而在 HTML 里这些换行本来就不可见 —— 折叠才是忠实的。
///
/// 真换行不靠这里：`<br>` 与块级标签（`div` / `p` / `li` …）由 [`Conv::on_tag`] 直接产出。
fn collapse_inline_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sp = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' || c == '\n' {
            sp = true;
        } else {
            if sp {
                out.push(' ');
            }
            sp = false;
            out.push(c);
        }
    }
    if sp {
        out.push(' ');
    }
    out
}

/// 行内文本的 Markdown 转义（覆盖会改变块结构或内联结构的最小集合）。
/// 不含 `|` —— 竖线在 shell 管道等正文里很常见，只在表格单元格里单独转义。
///
/// `<` **只在看起来像标签时**才转义（后面跟字母 / `/` / `!` / `?`）：CommonMark 会把
/// 这种 `<...>` 原样收进 HTML，渲染时被当标签丢掉 —— 实测为知笔记里有 12 篇的正文
/// （`<property>`、`<shape>`、`<?xml ...?>` 这类配置片段）因此整段消失。
/// 而 `a < b`、`x <= 1` 这种正常比较不会命中，md 源保持干净。
pub fn escape_md_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\\' | '*' | '_' | '`' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            '<' => {
                // 只有后跟字母 / `/` / `!` / `?` 才是标签形态。注意 html5ever 会把 `&lt;`
                // **单独切成一个字符 token**，单次调用看不到后文 —— 那个情形由
                // [`Conv::push_text`] 在下一段正文到达时补转义符。
                if matches!(it.peek(), Some(n)
                    if n.is_ascii_alphabetic() || *n == '/' || *n == '!' || *n == '?')
                {
                    out.push('\\');
                }
                out.push('<');
            }
            _ => out.push(c),
        }
    }
    out
}

// ============================ 单测 ============================

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        html_to_md(html)
    }

    // ---- M3 缺陷修复：行首排版缩进不得被 Markdown 当成「缩进代码块」----
    // 实测：md 库 1780 篇里 307 篇中招，渲染时凭空多出 13115 个代码块（正文显示成代码）。

    /// `is_md_block_start`：有语法含义的块起点（其缩进必须保留）
    #[test]
    fn test_is_md_block_start() {
        for keep in ["- x", "* x", "+ x", "1. x", "2) x", "> x", "| a | b |", "```sh", "## 标题"] {
            assert!(is_md_block_start(keep), "{keep:?} 应判为块起点");
        }
        for strip in ["Home", "-x", "1x", "a - b", "表格", "1.", "(x)"] {
            assert!(!is_md_block_start(strip), "{strip:?} 不该判为块起点");
        }
    }

    /// `dedent_layout_lines` 的取舍：排版缩进去掉、结构缩进保留、空白行归一
    #[test]
    fn test_dedent_layout_lines() {
        assert_eq!(dedent_layout_lines("    Home"), "Home");
        assert_eq!(dedent_layout_lines("\tHome"), "Home");
        assert_eq!(dedent_layout_lines("   Home"), "   Home", "不到 4 列不动");
        // 结构缩进保留（否则嵌套列表会被摊平）
        assert_eq!(dedent_layout_lines("    - x"), "    - x");
        assert_eq!(dedent_layout_lines("    > x"), "    > x");
        assert_eq!(dedent_layout_lines("        1. x"), "        1. x");
        // 只含空白的行 → 空行
        assert_eq!(dedent_layout_lines("a\n    \nb"), "a\n\nb");
        assert_eq!(
            dedent_layout_lines("        Home\n        Issues\n    - keep\n\n    - keep2"),
            "Home\nIssues\n    - keep\n\n    - keep2"
        );
    }

    /// 端到端：为知式「逐行 div + `&nbsp;` 缩进」不得渲染成代码块
    #[test]
    fn test_layout_indent_does_not_become_code_block() {
        let html = "<body>\
            <div>&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;Home</div>\
            <div><br></div>\
            <div>&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;Issues</div></body>";
        let m = md(html);
        assert!(
            !m.lines().any(|l| l.starts_with("    ")),
            "行首排版缩进应被去掉: {m:?}"
        );
        let out = md_to_html(&m);
        assert!(!out.contains("<pre>"), "正文被渲染成了代码块: {out}");
        assert!(out.contains("Home") && out.contains("Issues"), "{out}");
    }

    /// 反向护栏：围栏代码块**内部**的缩进必须逐字节保留（那是代码的一部分）
    #[test]
    fn test_dedent_never_touches_fence_content() {
        let html = "<body><pre>    def f():\n\treturn 1</pre></body>";
        let m = md(html);
        assert!(m.contains("    def f():"), "围栏内缩进被破坏: {m:?}");
        assert!(m.contains("\treturn 1"), "围栏内 Tab 被破坏: {m:?}");
        assert_eq!(
            m.matches("```").count(),
            2,
            "应是一个围栏: {m:?}"
        );
    }

    /// `collapse_inline_ws`：连续空白（含换行）折叠成一个空格，段首尾各留一个
    #[test]
    fn test_collapse_inline_ws() {
        assert_eq!(collapse_inline_ws("a\n   b"), "a b");
        assert_eq!(collapse_inline_ws("a\t\n\tb"), "a b");
        assert_eq!(collapse_inline_ws("   Home   "), " Home ");
        assert_eq!(collapse_inline_ws("Home"), "Home");
        assert_eq!(collapse_inline_ws("\n   "), " ");
    }

    /// M3 缺陷修复（转换器 v3）：**文本节点里的「缩进换行」不得变成缩进代码块**。
    ///
    /// 实测形态：GitHub 之类页面的 `<li><a>` 标签体被"缩进 + 换行"包裹（`<a>` 内还夹着
    /// 被忽略的 `<svg>` 之类元素，html5ever 把文本切成了多个 token），转换后 md 里留下
    /// 行首 4+ 空格的排版行；叠加列表标记后被 CommonMark 读成**缩进代码块**（md 库 258 篇中招）。
    #[test]
    fn text_node_newlines_collapse_and_do_not_make_code_blocks() {
        let html = "<body><ul>\
            <li><a href=\"https://x/\">\n        \n          Home\n      </a></li>\
            <li><a href=\"https://x/i\">\n          Issues\n      </a></li>\
            </ul></body>";
        let m = md(html);
        assert!(
            !m.lines().any(|l| l.starts_with("    ")),
            "文本节点的排版缩进应被折叠掉: {m:?}"
        );
        let out = md_to_html(&m);
        assert!(!out.contains("<pre"), "正文被渲染成了代码块: {out}");
        assert!(out.contains("Home") && out.contains("Issues"), "{out}");
        assert!(out.contains("https://x/") && out.contains("https://x/i"), "{out}");
    }

    /// 转换器 v3：嵌套列表标记最多缩进 **2 空格**（≥4 会被 CommonMark 读成缩进代码块）
    #[test]
    fn nested_list_markers_indent_at_most_two_spaces() {
        let html = "<body><ul><li>一级<ul><li>二级<ul><li>三级</li></ul></li></ul></li></ul></body>";
        let m = md(html);
        assert!(
            !m.lines().any(|l| l.starts_with("    ")),
            "列表标记缩进超过了 2 列: {m:?}"
        );
        let out = md_to_html(&m);
        assert!(!out.contains("<pre"), "列表被渲染成了代码块: {out}");
        for t in ["一级", "二级", "三级"] {
            assert!(out.contains(t), "少了 {t}: {out}");
        }
    }

    #[test]
    fn test_div_lines_do_not_merge() {
        // 为知正文是 div 平铺，每个 div 是一行 —— 绝不能糊成一整段
        let html = "<body><div>line1</div><div>line2</div><div>line3</div></body>";
        let m = md(html);
        assert!(m.contains("line1"), "{m}");
        assert!(m.contains("line2"), "{m}");
        assert!(m.contains("line3"), "{m}");
        let plain: String = m.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(plain.contains("line1") && plain.contains("line2"));
    }

    #[test]
    fn test_code_block_source_from_hidden_textarea() {
        let html = r#"<body><div class="wiz-code-container" data-mode="shell"><textarea style="display: none;">echo hi
ls -l</textarea><wiz_code_mirror><div class="CodeMirror"><pre class="CodeMirror-line">echo hi</pre><pre class="CodeMirror-line">ls -l</pre></div></wiz_code_mirror></div></body>"#;
        let m = md(html);
        assert!(m.contains("```shell"), "{m}");
        assert!(m.contains("echo hi"), "{m}");
        assert!(m.contains("ls -l"), "{m}");
        // 源码只出现一次：镜像没有被当成正文留下
        assert_eq!(m.matches("echo hi").count(), 1, "代码内容重复了: {m}");
        assert_eq!(m.matches("```").count(), 2, "围栏不成对: {m}");
    }

    #[test]
    fn test_adjacent_code_blocks_not_glued() {
        // §20.5 缺陷 ①：两个相邻代码块不得粘成 `-----``````shell`
        let html = r#"<body><div class="wiz-code-container" data-mode="">
<textarea style="display:none;">-----</textarea><wiz_code_mirror><pre class="CodeMirror-line">-----</pre></wiz_code_mirror></div>
<div class="wiz-code-container" data-mode="shell"><textarea style="display:none;">echo a</textarea><wiz_code_mirror><pre class="CodeMirror-line">echo a</pre></wiz_code_mirror></div></body>"#;
        let m = md(html);
        assert!(!m.contains("-----```"), "围栏粘连: {m}");
        assert!(m.contains("```\n-----\n```"), "{m}");
        assert!(m.contains("```shell\necho a\n```"), "{m}");
    }

    #[test]
    fn test_empty_code_block_dropped() {
        let html = r#"<body><div class="wiz-code-container" data-mode="js"><textarea style="display:none;">   </textarea><wiz_code_mirror><pre class="CodeMirror-line"></pre></wiz_code_mirror></div></body>"#;
        let m = md(html);
        assert!(!m.contains("```"), "空围栏应被丢弃: {m}");
    }

    #[test]
    fn test_table_to_gfm() {
        let html = "<body><table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table></body>";
        let m = md(html);
        assert!(m.contains("| a | b |"), "{m}");
        assert!(m.contains("| --- | --- |"), "{m}");
        assert!(m.contains("| 1 | 2 |"), "{m}");
    }

    #[test]
    fn test_heading_list_emphasis_link() {
        let html = r#"<body><h2>标题</h2><ul><li>一</li><li><strong>二</strong></li></ul><p><em>斜</em> <a href="https://x.com">链</a></p></body>"#;
        let m = md(html);
        assert!(m.contains("## 标题"), "{m}");
        assert!(m.contains("- 一"), "{m}");
        assert!(m.contains("- **二**"), "{m}");
        assert!(m.contains("*斜*"), "{m}");
        assert!(m.contains("[链](https://x.com)"), "{m}");
    }

    /// 回归（严重）：void 元素没有结束标签，绝不能被计入忽略深度。
    /// 修复前 `<meta>`/`<link>` 会让 ignore 永远 ≥ 1，元数据之后的正文全被吞掉，
    /// 实测 814/1780 篇产出 0 字节。
    #[test]
    fn test_meta_link_do_not_swallow_document() {
        let html = "<html><head><meta charset=\"utf-8\"><meta name=\"a\" content=\"b\"><link rel=\"stylesheet\" href=\"index_files/x.css\"></head><body><div>正文第一行</div><div>正文第二行</div></body></html>";
        let m = md(html);
        assert!(m.contains("正文第一行"), "正文被吞: {m}");
        assert!(m.contains("正文第二行"), "正文被吞: {m}");
    }

    /// 回归：`</head>` 缺失时，`<body>` 必须把 head 状态收回来
    #[test]
    fn test_missing_head_close_recovers_at_body() {
        let html = "<html><head><title>t</title><body><div>内容甲</div></body></html>";
        let m = md(html);
        assert!(m.contains("内容甲"), "正文被吞: {m}");
    }

    #[test]
    fn test_style_script_stripped() {
        let html = "<html><head><style>a{color:red}</style><title>t</title></head><body><script>var x=1</script><div>真内容</div></body></html>";
        let m = md(html);
        assert!(m.contains("真内容"), "{m}");
        assert!(!m.contains("color:red"), "{m}");
        assert!(!m.contains("var x"), "{m}");
    }

    #[test]
    fn test_data_uri_image_dropped() {
        let html = r#"<body><div>前</div><img src="data:image/png;base64,AAAA" alt="qr"><div>后</div></body>"#;
        let (m, st) = html_to_md_with_stats(html);
        assert!(!m.contains("data:image"), "{m}");
        assert_eq!(st.dropped_data_imgs, 1);
    }

    #[test]
    fn test_local_image_kept_with_relative_path() {
        let html = r#"<body><img src="index_files/p1.png" alt="图"></body>"#;
        let m = md(html);
        assert!(m.contains("![图](index_files/p1.png)"), "{m}");
    }

    #[test]
    fn test_nbsp_normalized() {
        let html = "<body><div>a&nbsp;&nbsp;b</div></body>";
        let m = md(html);
        assert!(!m.contains('\u{a0}'), "{m}");
        // 连续 `&nbsp;` 折叠成**一个**空格：HTML 渲染时连续空白本就折叠，
        // md 里保留多个空格只会制造"行首 4+ 空格排版行"（见 `collapse_inline_ws`）
        assert!(m.contains("a b"), "{m}");
    }

    #[test]
    fn test_md_to_html_roundtrip() {
        let html = md_to_html("# 标题\n\n| a | b |\n| --- | --- |\n| 1 | 2 |\n\n```shell\nls\n```\n");
        assert!(html.contains("<h1>"), "{html}");
        assert!(html.contains("<table>"), "{html}");
        assert!(html.contains("<pre><code class=\"language-shell\">"), "{html}");
        assert!(html.contains("<td>1</td>"), "{html}");
    }

    #[test]
    fn test_document_has_charset_and_title() {
        let doc = md_to_html_document("正文", "标题<&>");
        assert!(doc.starts_with("<!DOCTYPE html>"), "{doc}");
        assert!(doc.contains("<meta charset=\"utf-8\">"), "{doc}");
        assert!(doc.contains("标题&lt;&amp;&gt;"), "标题未转义: {doc}");
        assert!(doc.contains("正文"), "{doc}");
        // 阅读态的两条契约（改动这里要同步 `sandbox::COMPAT_JS`）：
        // ① `body.md-body` 是兼容层识别"md 包笔记"的**唯一标记**（据此补代码块复制按钮）；
        // ② 必须有 `</body>`，宿主靠它把兼容层脚本注入进去（`inject_before_body_close`）。
        assert!(doc.contains("<body class=\"md-body\">"), "md-body 标记丢了: {doc}");
        assert!(doc.contains("</body></html>"), "缺 </body> 则兼容层注入不上: {doc}");
    }

    #[test]
    fn test_blockquote_prefixed() {
        let html = "<body><blockquote><div>引用一</div><div>引用二</div></blockquote></body>";
        let m = md(html);
        assert!(m.contains("> 引用一"), "{m}");
        assert!(m.contains("> 引用二"), "{m}");
    }

    #[test]
    fn test_table_with_nested_div_in_cell() {
        // 为知表格单元格里常裹一层 div
        let html = "<body><table><tr><td><div>甲<br>乙</div></td><td>丙</td></tr></table></body>";
        let m = md(html);
        assert!(m.contains("甲<br>乙"), "{m}");
        assert!(m.contains("丙"), "{m}");
    }

    // ---------- 代码排版表（用 `<table>` 摆"行号 + 代码"） ----------

    /// Hugo / Chroma 的 `lntable`：一个围栏、行号槽丢掉、代码**不做 md 转义**
    #[test]
    fn test_lntable_becomes_fence_without_escaping() {
        let html = "<body><div class=\"highlight\"><div class=\"chroma\">\
<table class=\"lntable\"><tbody><tr>\
<td class=\"lntd\"><pre class=\"chroma\"><span class=\"lnt\">1\n</span><span class=\"lnt\">2\n</span></pre></td>\
<td class=\"lntd\"><pre class=\"chroma\">[mons] = 0\necho done</pre></td>\
</tr></tbody></table></div></div></body>";
        let (m, st) = html_to_md_with_stats(html);
        assert_eq!(st.code_tables, 1, "{m}");
        assert_eq!(st.tables, 0, "代码排版表不该算数据表格: {m}");
        assert_eq!(m.matches("```").count(), 2, "围栏不成对: {m}");
        assert!(m.contains("[mons] = 0"), "代码被 md 转义了: {m}");
        assert!(!m.contains("\\["), "代码被 md 转义了: {m}");
        assert!(m.contains("echo done"), "{m}");
        // 行号槽不能混进代码
        assert!(!m.contains("```\n1\n2"), "行号进了围栏: {m}");
        assert!(!m.contains("| 1 |"), "渲染成数据表格了: {m}");
    }

    /// highlight.js 的 `hljs-ln`：N 行 × (行号, 代码)，空行必须保住
    #[test]
    fn test_hljs_ln_keeps_blank_line() {
        let html = "<body><table class=\"hljs-ln\"><tbody>\
<tr><td class=\"hljs-ln-line hljs-ln-numbers\"><div class=\"hljs-ln-n\"></div></td><td class=\"hljs-ln-line hljs-ln-code\">alpha</td></tr>\
<tr><td class=\"hljs-ln-line hljs-ln-numbers\"><div class=\"hljs-ln-n\"></div></td><td class=\"hljs-ln-line hljs-ln-code\"></td></tr>\
<tr><td class=\"hljs-ln-line hljs-ln-numbers\"><div class=\"hljs-ln-n\"></div></td><td class=\"hljs-ln-line hljs-ln-code\">omega</td></tr>\
</tbody></table></body>";
        let m = md(html);
        assert!(m.contains("```\nalpha\n\nomega\n```"), "空行没保住: {m}");
    }

    /// Crayon 的 `crayon-table`：行号槽 class 含 `nums`
    #[test]
    fn test_crayon_table_becomes_fence() {
        let html = "<body><table class=\"crayon-table\"><tbody><tr>\
<td class=\"crayon-nums\"><div class=\"crayon-num\">1</div><div class=\"crayon-num\">2</div></td>\
<td class=\"crayon-code\"><div class=\"crayon-line\">first</div><div class=\"crayon-line\">second</div></td>\
</tr></tbody></table></body>";
        let m = md(html);
        assert!(m.contains("```\nfirst\nsecond\n```"), "{m}");
    }

    /// GitHub 统一 diff 视图：`blob-num` 空行号格 + `blob-code` 内容，语言 `diff`
    #[test]
    fn test_github_diff_table_becomes_diff_fence() {
        let html = "<body><table class=\"diff-table js-diff-table tab-size\"><tbody>\
<tr><td class=\"blob-num blob-num-context\"></td><td class=\"blob-code blob-code-inner\"> keep me</td></tr>\
<tr><td class=\"blob-num blob-num-deletion\"></td><td class=\"blob-code blob-code-inner\">-gone</td></tr>\
</tbody></table></body>";
        let m = md(html);
        assert!(m.contains("```diff"), "{m}");
        assert!(m.contains(" keep me"), "{m}");
        assert!(m.contains("-gone"), "{m}");
        assert!(!m.contains("| --- |"), "渲染成数据表格了: {m}");
    }

    /// `d-block` 单列包裹表：内容要**摊平**成普通块级元素（段落/引用/列表照常生效）
    #[test]
    fn test_layout_table_flattened_keeps_blocks() {
        let html = "<body><div>前</div><table class=\"d-block user-select-contain\"><tbody class=\"d-block\">\
<tr class=\"d-block\"><td class=\"d-block comment-body markdown-body\"><p>一段评论</p><blockquote><p>引一句</p></blockquote><ul><li>条目</li></ul></td></tr>\
</tbody></table><div>后</div></body>";
        let (m, st) = html_to_md_with_stats(html);
        assert_eq!(st.layout_tables, 1, "{m}");
        assert_eq!(st.tables, 0, "布局表不该产出数据表格: {m}");
        assert!(m.contains("一段评论"), "{m}");
        assert!(m.contains("> 引一句"), "引用结构丢了: {m}");
        assert!(m.contains("- 条目"), "列表结构丢了: {m}");
        assert!(!m.contains("| --- |"), "渲染成数据表格了: {m}");
        assert!(m.contains("后"), "布局表后的正文被吞: {m}");
    }

    /// 布局表结束后，外层元素的配对不能被破坏（`</table>` 不能去 pop 外层标签）
    #[test]
    fn test_layout_table_does_not_unbalance_stack() {
        let html = "<body><div>甲<table class=\"d-block\"><tbody><tr><td><p>乙</p></td></tr></tbody></table>丙</div></body>";
        let m = md(html);
        assert!(m.contains('甲'), "{m}");
        assert!(m.contains('乙'), "{m}");
        assert!(m.contains('丙'), "{m}");
    }

    /// 真数据表格不受影响
    #[test]
    fn test_data_table_still_table() {
        let html = "<body><table class=\"dataframe\"><tr><th>x</th><th>y</th></tr><tr><td>1</td><td>2</td></tr></table></body>";
        let (m, st) = html_to_md_with_stats(html);
        assert_eq!(st.tables, 1, "{m}");
        assert_eq!(st.code_tables, 0, "{m}");
        assert!(m.contains("| x | y |"), "{m}");
        assert!(m.contains("| 1 | 2 |"), "{m}");
    }

    /// 合并单元格：GFM 表达不了 ⇒ 降级为纯文本行（而不是产出一张错表）
    #[test]
    fn test_colspan_degrades_to_text_lines() {
        let html = "<body><table><tr><td colspan=\"2\">跨两列</td></tr><tr><td>a</td><td>b</td></tr></table></body>";
        let m = md(html);
        assert!(m.contains("跨两列"), "{m}");
        assert!(!m.contains("| --- |"), "colspan 不该硬凑成 GFM 表: {m}");
    }

    // ---------- 裸 `<pre>`（不是为知代码容器） ----------

    /// 回归（严重）：裸 `<pre>` 的 `</pre>` 必须自己收口。
    ///
    /// 修复前 `on_tag` 在 `self.pre.is_some()` 时**对所有标签**早退，`</pre>` 也被吞掉，
    /// `self.pre` 永远是 `Some` ⇒ 该 `<pre>` 之后（含它自己）的整篇正文全丢。
    /// 实测 1780 篇里有 20 篇因此产出 0 字节。
    #[test]
    fn test_bare_pre_does_not_swallow_rest_of_note() {
        let html = "<body><pre>echo hello\nls -l</pre><div>后面还有正文</div><div>第二行</div></body>";
        let (m, st) = html_to_md_with_stats(html);
        assert_eq!(st.code_blocks, 1, "{m}");
        assert!(m.contains("echo hello"), "{m}");
        assert!(m.contains("ls -l"), "{m}");
        assert!(m.contains("后面还有正文"), "`</pre>` 之后的正文被吞: {m}");
        assert!(m.contains("第二行"), "`</pre>` 之后的正文被吞: {m}");
        assert_eq!(m.matches("```").count(), 2, "围栏不成对: {m}");
    }

    /// 裸 `<pre>` 的语言取显式前缀，认不出就空（不能把 `wiz-editor-body` 当语言）
    #[test]
    fn test_bare_pre_language_hint() {
        let m = md("<body><pre class=\"line-numbers language-python\">x = 1</pre></body>");
        assert!(m.contains("```python"), "{m}");
        let m2 = md("<body><pre>y = 2</pre></body>");
        assert!(m2.contains("```\ny = 2\n```"), "{m2}");
    }

    /// 裸 `<pre>` 之后紧跟的 `<br>` 要变成真换行，且不产生内联标记
    #[test]
    fn test_bare_pre_br_and_no_inline_marks() {
        let html = "<body><pre>a<br>b <em>c</em></pre></body>";
        let m = md(html);
        assert!(m.contains("a\nb"), "{m}");
        assert!(!m.contains("*c*"), "`<pre>` 内不该产出内联标记: {m}");
    }

    // ---------- `<` 与原始 HTML ----------

    /// 回归（严重）：正文里形如标签的 `<...>` 必须转义。
    ///
    /// 不转义时 CommonMark 会把它当原始 HTML 收下，渲染阶段被当标签丢掉 ——
    /// 实测 12 篇配置类笔记（`<property>` / `<shape>` / `<?xml ...?>`) 的正文整段消失。
    #[test]
    fn test_angle_bracket_tag_like_text_escaped() {
        let html = "<body><div>&lt;property&gt;</div><div>&nbsp; &lt;name&gt;yarn&lt;/name&gt;</div><div>a &lt; b 不受影响</div></body>";
        let m = md(html);
        assert!(m.contains("\\<property>") || m.contains("\\<property\\>"), "{m}");
        assert!(m.contains("a < b"), "正常比较不该被转义: {m}");
        // 渲染回 HTML 后文字必须还在
        let h = md_to_html(&m);
        assert!(h.contains("&lt;property&gt;") || h.contains("<p>&lt;property&gt;"), "渲染后正文丢了: {h}");
    }

    /// `<?xml` 这种行首 PI 也要收住，否则后续整块被当 HTML 吞掉
    #[test]
    fn test_processing_instruction_escaped() {
        let m = md("<body><div>&lt;?xml version=\"1.0\"?&gt;</div><div>后面的正文</div></body>");
        assert!(m.contains("\\<?xml"), "{m}");
        assert!(m.contains("后面的正文"), "{m}");
    }

    /// 回归：表格单元格里的 `<` 同样要补转义。
    ///
    /// 单元格与正文是**两套缓冲**（`tb.cell` / `self.cur`），只补正文会漏掉单元格里的
    /// 整段 XML —— 实测 1 篇 cosbench 配置笔记渲染后 5900 字 XML 全无。
    #[test]
    fn test_angle_escape_inside_table_cell() {
        let html = "<body><table><tr><td>&lt;?xml version=\"1.0\"?&gt;&lt;root a=\"b\"/&gt;</td></tr>\
<tr><td>普通格</td></tr></table></body>";
        let m = md(html);
        assert!(m.contains("\\<?xml"), "单元格里的裸 `<` 没补转义: {m}");
        assert!(m.contains("\\<root"), "{m}");
        let h = md_to_html(&m);
        assert!(h.contains("&lt;?xml"), "渲染后单元格内容丢了: {h}");
    }

    /// 回归：嵌套 `<table>` 的内层 `</table>` 不得提前关掉外层表，
    /// 内层结构标签也不能关掉外层的"正在单元格"状态（否则后续正文没人接管）。
    #[test]
    fn test_nested_table_does_not_close_outer() {
        let html = "<body><table><tr><td>投票列</td><td>正文<div>答:看 updatev</div>\
<table><tr><td>嵌套内层格</td></tr></table>正文尾巴</td></tr></table><div>表后正文</div></body>";
        let (m, st) = html_to_md_with_stats(html);
        assert_eq!(st.tables, 1, "只应产出一张表: {m}");
        assert!(m.contains("投票列"), "{m}");
        assert!(m.contains("答:看 updatev"), "外层单元格内容丢了: {m}");
        assert!(m.contains("嵌套内层格"), "内层表内容丢了: {m}");
        assert!(m.contains("正文尾巴"), "内层表之后的正文丢了: {m}");
        assert!(m.contains("表后正文"), "表后正文丢了: {m}");
    }

    /// 回归：`<pre>` 里的"行号槽 / 复制按钮"不能混进围栏。
    ///
    /// 实测 CSDN 剪藏的代码块把 `<ul class="pre-numbering"><li>1</li>…</ul>` 塞在
    /// `<pre>` 内部，早期实现把 1..19 全部收下，围栏末尾多出一串 `12345…19`。
    #[test]
    fn test_pre_chrome_line_numbers_skipped() {
        let html = "<body><pre class=\"prettyprint\"><code class=\"prism language-shell has-numbering\">\
echo one\n<span class=\"token\">echo</span> two\
<div class=\"hljs-button signin\" data-title=\"登录后复制\"></div></code>\
<ul class=\"pre-numbering\"><li>1</li><li>2</li><li>3</li></ul></pre></body>";
        let m = md(html);
        assert!(m.contains("```shell"), "{m}");
        assert!(m.contains("echo one"), "{m}");
        assert!(m.contains("echo two"), "{m}");
        assert!(!m.contains("123"), "行号混进围栏: {m}");
        assert!(!m.contains("登录后复制"), "复制按钮混进围栏: {m}");
        assert_eq!(m.matches("```").count(), 2, "围栏不成对: {m}");

        // 行号槽 `<ul>` 只有 style、没有 class ⇒ 靠"`<li>` 当换行 + 末尾纯数字行整段剪掉"识别
        let html2 = "<body><pre>real code\n<ul style=\"left: 0px;\"><li>1</li><li>2</li><li>3</li></ul>\
<button type=\"button\">AI写代码</button></pre></body>";
        let m2 = md(html2);
        assert!(m2.contains("real code"), "{m2}");
        assert!(!m2.contains("123"), "无 class 的行号槽没剪掉: {m2}");
        assert!(!m2.contains("AI写代码"), "无 class 的按钮没跳过: {m2}");
        assert_eq!(m2.trim().matches("```").count(), 2, "{m2}");
    }

    /// **边界（踩过一次）**：CSDN 有个变体把**代码本体**写成
    /// `<pre><code><ol><li>一行代码</li>…</ol></code></pre>`。
    /// 早期版本按"`<pre>` 里的 ul/ol 必是零件"整块跳过 ⇒ 整段代码被删，9 篇覆盖率崩到 0.06。
    #[test]
    fn test_pre_ol_li_is_code_not_chrome() {
        let html = "<body><pre class=\"prettyprint\"><code class=\"language-shell\"><ol>\
<li>ceph -s</li><li>ceph osd tree</li><li>ceph df</li></ol></code></pre></body>";
        let m = md(html);
        assert!(m.contains("```shell"), "{m}");
        assert!(m.contains("ceph -s"), "代码被当零件删了: {m}");
        assert!(m.contains("ceph osd tree"), "{m}");
        assert!(m.contains("ceph df"), "{m}");
        // 三行必须是三行，不能粘成一行
        assert!(m.contains("ceph -s\nceph osd tree\nceph df"), "行没分开: {m}");
    }

    /// 剪行号的两条保护：连续 <2 行不剪；代码末行恰好是数字时不被误伤
    #[test]
    fn test_strip_trailing_line_numbers_guards() {
        assert_eq!(strip_trailing_line_numbers("a\nb\n42"), "a\nb\n42");
        assert_eq!(strip_trailing_line_numbers("a\nb\n1\n2\n3"), "a\nb");
        assert_eq!(strip_trailing_line_numbers("a\nb\n1\n2\n3\n\n"), "a\nb");
        assert_eq!(strip_trailing_line_numbers("1\n2"), "");
    }

    /// 上面那条的**边界**：`line-numbers` / `has-numbering` 打在代码本体上时，
    /// 代码块必须完整保留（70 篇 Prism/highlight.js 剪藏是这样写的）
    #[test]
    fn test_pre_body_level_line_numbers_class_not_skipped() {
        let html = "<body><pre class=\"line-numbers language-js\"><code class=\"has-numbering\">var a = 1</code></pre></body>";
        let m = md(html);
        assert!(m.contains("var a = 1"), "代码块被误跳过了: {m}");
    }

    /// 转义函数本身的行为（隔离验证，避免上游 token 化干扰判断）
    #[test]
    fn test_escape_md_text_angle_rules() {
        assert_eq!(escape_md_text("<property>"), "\\<property>");
        assert_eq!(escape_md_text("</div>"), "\\</div>");
        assert_eq!(escape_md_text("<?xml?>"), "\\<?xml?>");
        assert_eq!(escape_md_text("<!-- x -->"), "\\<!-- x -->");
        // `&lt;` 会独立成 token ⇒ `escape_md_text` 单看它是末字符、看不到后文，故不转义；
        // 由 `push_text` 在下一段正文到达时补（见 test_angle_bracket_tag_like_text_escaped）
        assert_eq!(escape_md_text("<"), "<");
        // 非标签形态不转义（同一次调用里能看到后文时）
        assert_eq!(escape_md_text("a < b"), "a < b");
        assert_eq!(escape_md_text("x <= 1"), "x <= 1");
        assert_eq!(escape_md_text("5 <6"), "5 <6");
    }
}
