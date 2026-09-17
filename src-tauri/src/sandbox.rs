//! WebView 沙箱与宿主兼容层（NFR-3.5 / FR-05.4）
//!
//! 隔离靠三件东西，缺一即失守：
//! 1. **CSP**：`default-src 'none'` 打底，远程资源仅在用户显式开关下放开；
//!    `form-action 'none'` 禁表单提交、`frame-src 'none'` 禁嵌套、`object-src 'none'` 禁插件。
//! 2. **nonce**：`script-src 'nonce-…'`（**只给 nonce，不给任何 URL 源**）。实测全库有 6 篇笔记
//!    正文含 41 个 `<script>`（其中 8 个带 `src`，指向包内 js）；若写成
//!    `script-src 'nonce-…' wiznote:`，包内 js 会被放行。因此兼容层直接内联注入，
//!    笔记自带的内联/外部脚本一律阻断。
//! 3. **iframe sandbox**：前端 `Reader.vue` 的 iframe 上再加一层 `allow-scripts`，
//!    禁同源访问、禁顶层跳转、禁弹窗。

/// 每篇笔记响应注入的兼容层 JS（T0.3/T2.3/T2.5，FR-05.4）
/// - 形态 A：textarea → `<pre><code>`；**已有 CodeMirror 序列化渲染结果的容器跳过**
///   （M4 巡检实测：1,533 个源码 textarea 中 1,496 个同时带渲染结果，若照单全换，代码会显示两遍）
/// - 每个代码块一键复制（S3 最高频动作）
/// - R9：形态 B CodeMirror DOM 复制归一化
/// - NFR-3.3：外链一律系统浏览器（经 wiznote-action 协议）
pub const COMPAT_JS: &str = r#"(function(){
  'use strict';
  function copyText(text, btn){
    function done(){ if(btn){ btn.textContent='已复制'; setTimeout(function(){btn.textContent='复制';},1200);} }
    function fallback(){
      var t=document.createElement('textarea');
      t.value=text; t.style.cssText='position:fixed;opacity:0;';
      document.body.appendChild(t); t.select();
      try{ document.execCommand('copy'); done(); }catch(e){}
      t.remove();
    }
    if(navigator.clipboard && navigator.clipboard.writeText){
      navigator.clipboard.writeText(text).then(done).catch(fallback);
    } else { fallback(); }
  }
  /* 是否已序列化渲染：找非测量用途的 CodeMirror-line（.CodeMirror-measure 内的是 xxx 占位） */
  function renderedLines(box){
    var pres=box.querySelectorAll('pre');
    for(var i=0;i<pres.length;i++){
      var p=pres[i];
      if(!p.classList.contains('CodeMirror-line')) continue;
      if(p.closest && p.closest('.CodeMirror-measure')) continue;
      if(p.textContent.trim().length) return true;
    }
    return false;
  }
  /* 源码 textarea：display:none 且有内容（CodeMirror 输入框 textarea 为空，排除） */
  function sourceArea(box){
    var tas=box.querySelectorAll('textarea');
    for(var i=0;i<tas.length;i++){
      var t=tas[i];
      var d='';
      try{ d=getComputedStyle(t).display; }catch(e){}
      if((/none/i.test(t.getAttribute('style')||'') || d==='none') && t.value && t.value.trim().length) return t;
    }
    return null;
  }
  function decorate(box, pre, code){
    pre.style.cssText='margin:0;overflow-x:auto;white-space:pre;';
    var btn=document.createElement('button');
    btn.textContent='复制';
    btn.style.cssText='position:absolute;top:4px;right:8px;font-size:12px;padding:2px 8px;cursor:pointer;border:1px solid #ccc;border-radius:4px;background:#fff;color:#333;z-index:9;';
    btn.addEventListener('click',function(){ copyText(code.textContent, btn); });
    box.style.position='relative';
    box.appendChild(btn);
  }
  function convert(){
    document.querySelectorAll('.wiz-code-container').forEach(function(box){
      if(box.dataset.wizProcessed) return;
      box.dataset.wizProcessed='1';
      if(renderedLines(box)) return;         /* 已是渲染态：不动，避免代码出现两遍 */
      var ta=sourceArea(box);
      if(!ta) return;
      var pre=document.createElement('pre');
      var code=document.createElement('code');
      var lang=(box.dataset.mode||'').toLowerCase();
      if(lang) code.className='language-'+lang;
      code.textContent=ta.value;             /* 浏览器已自动反转义实体 */
      pre.appendChild(code);
      ta.replaceWith(pre);
      decorate(box, pre, code);
    });
  }
  /* R9：形态 B 的 CodeMirror DOM（多层 span + 绝对定位）复制归一化 */
  document.addEventListener('copy', function(e){
    var sel=document.getSelection();
    if(!sel || sel.isCollapsed || !sel.anchorNode) return;
    var node = sel.anchorNode.nodeType===1 ? sel.anchorNode : sel.anchorNode.parentElement;
    var cc = node && node.closest ? node.closest('.wiz-code-container') : null;
    if(!cc || !renderedLines(cc)) return;    /* 只归一化渲染态；形态 A 已是干净 <pre> */
    var lines=Array.prototype.slice.call(cc.querySelectorAll('pre')).filter(function(p){
      return p.classList.contains('CodeMirror-line') && !(p.closest && p.closest('.CodeMirror-measure'));
    }).map(function(l){return l.textContent;});
    if(lines.length){
      e.clipboardData.setData('text/plain', lines.join('\n'));
      e.preventDefault();
    }
  });
  /* NFR-3.3：远程链接一律系统浏览器，绝不入 WebView */
  document.addEventListener('click', function(e){
    var a = e.target && e.target.closest ? e.target.closest('a') : null;
    if(!a) return;
    var href=a.getAttribute('href')||'';
    if(/^(https?:)?\/\//i.test(href)){
      e.preventDefault();
      var abs = href.indexOf('//')===0 ? 'https:'+href : href;
      var img=new Image();
      img.src='wiznote-action://open-url?url='+encodeURIComponent(abs);
    }
  });
  if(document.readyState==='loading'){
    document.addEventListener('DOMContentLoaded', convert);
  } else { convert(); }
})();
"#;

/// 生成一次性 nonce（CSP `script-src`）。
/// 用进程内随机种子 + 纳秒时戳哈希，无需引入 rand 依赖；
/// 威胁模型是"阻断笔记正文自带的脚本"，不需密码学强度。
pub fn nonce() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    static SEED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let seed = *SEED.get_or_init(|| {
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        h.finish()
    });
    let mut h = DefaultHasher::new();
    seed.hash(&mut h);
    std::time::Instant::now().elapsed().as_nanos().hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}", h.finish())
}

/// 笔记文档的 CSP（NFR-3.2 / NFR-3.5）
///
/// `script-src` 只列 nonce：笔记自带的内联脚本与包内 js 均执行不了，
/// 只有 [`compat_script_tag`] 内联注入的那一枚可运行。
pub fn note_csp(nonce: &str, allow_remote: bool) -> String {
    let img = if allow_remote {
        "img-src wiznote: data: blob: http: https:"
    } else {
        "img-src wiznote: data: blob:"
    };
    format!(
        "default-src 'none'; {}; style-src wiznote: data: 'unsafe-inline'; \
         font-src wiznote: data:; script-src 'nonce-{nonce}'; \
         form-action 'none'; frame-src 'none'; object-src 'none'; media-src wiznote: data:; \
         base-uri 'none'; manifest-src 'none'; worker-src 'none'; \
         connect-src wiznote-action:;",
        img
    )
}

/// 宿主注入的兼容层：内联带 nonce 的 `<script>`（避开 `script-src` 开放 URL 源）
pub fn compat_script_tag(nonce: &str) -> String {
    debug_assert!(!COMPAT_JS.contains("</script"), "内联脚本不得提前闭合");
    format!("<script nonce=\"{nonce}\">{COMPAT_JS}</script>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csp_blocks_remote_images_by_default() {
        let csp = note_csp("x", false);
        assert!(csp.starts_with("default-src 'none';"));
        assert!(csp.contains("img-src wiznote: data: blob:"));
        assert!(!csp.contains("http:"), "默认必须阻断远程资源：{csp}");
    }

    #[test]
    fn csp_allows_remote_only_when_opted_in() {
        assert!(note_csp("x", true).contains("img-src wiznote: data: blob: http: https:"));
    }

    /// NFR-3.5：笔记内 JS 必须被禁，只有宿主注入的兼容层可运行
    #[test]
    fn csp_disables_note_scripts_via_nonce() {
        let csp = note_csp("abc123", false);
        let script = csp
            .split("; ")
            .find(|s| s.starts_with("script-src"))
            .expect("必须有 script-src");
        assert_eq!(script, "script-src 'nonce-abc123'", "script-src 不得列任何 URL 源：{script}");
        assert!(!script.contains("'unsafe-inline'"));
        assert!(!script.contains("'unsafe-eval'"));
        assert!(!script.contains("wiznote:"), "包内 js 不得放行：{script}");
        // 内联样式必须放开（笔记正文自带 <style>），但不影响脚本
        assert!(csp.contains("style-src wiznote: data: 'unsafe-inline'"));
        assert!(csp.contains("form-action 'none'"));
        assert!(csp.contains("frame-src 'none'"));
        assert!(csp.contains("object-src 'none'"));
        assert!(csp.contains("base-uri 'none'"));
    }

    /// 兼容层内联注入，标签自身可安全嵌入 HTML
    #[test]
    fn compat_tag_is_inline_and_closed() {
        let tag = compat_script_tag("n0nce");
        assert!(tag.starts_with("<script nonce=\"n0nce\">"));
        assert!(tag.ends_with("</script>"));
        assert_eq!(tag.matches("</script").count(), 1);
    }

    #[test]
    fn nonce_is_unique_per_call() {
        assert_ne!(nonce(), nonce());
    }

    #[test]
    fn compat_js_skips_rendered_containers() {
        assert!(COMPAT_JS.contains("renderedLines(box)"));
        assert!(COMPAT_JS.contains("CodeMirror-measure"));
    }
}
