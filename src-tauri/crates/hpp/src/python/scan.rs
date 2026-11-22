//! Python 蜘蛛源码的静态扫描：内容识别与方法面探测。
//!
//! 只做纯文本扫描，不执行任何 Python 代码。执行期的方法面以边车
//! /load 返回的为准；这里的扫描只决定“这段文本是不是 Python 蜘蛛”
//! 以及“它有没有至少一个内容方法”，供导入预览与启动恢复使用。
use crate::{PluginError, Result};
use regex::Regex;
use std::sync::OnceLock;

/// TVBox 蜘蛛方法名（类式与函数式共用同一套名字）。
pub const SPIDER_METHODS: &[&str] = &[
    "init",
    "homeContent",
    "homeVideoContent",
    "categoryContent",
    "detailContent",
    "searchContent",
    "playerContent",
    "localProxy",
    "liveContent",
    "action",
    "destroy",
    "isVideoFormat",
    "manualVideoCheck",
    "getName",
    "getDependence",
];

/// 至少实现其中一个才算内容源。
const CONTENT_METHODS: &[&str] = &[
    "homeContent",
    "homeVideoContent",
    "categoryContent",
    "detailContent",
    "searchContent",
    "playerContent",
];

/// 静态扫描出的蜘蛛面：写法 + 检测到的方法名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpiderFace {
    /// 类式（`class Spider`）或函数式（模块级函数）。
    pub class_style: bool,
    /// 检测到的方法名（去重、按 SPIDER_METHODS 的顺序）。
    pub methods: Vec<String>,
}

fn class_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[ \t]*class[ \t]+Spider[ \t]*\(").unwrap())
}

fn def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^[ \t]*def[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*\(").unwrap()
    })
}

fn module_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?m)^def[ \t]+([A-Za-z_][A-Za-z0-9_]*)[ \t]*\(").unwrap()
    })
}

fn get_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?m)^[ \t]*def[ \t]+getName[ \t]*\([^)]*\)[^:]*:[ \t]*\n(?:[ \t]*#[^\n]*\n|[ \t]*\n)*[ \t]*return[ \t]+["']([^"']+)["']"#)
            .unwrap()
    })
}

/// 文本里出现的方法名（任意缩进的 `def 名(`，含类方法与模块函数）。
fn defined_methods(code: &str) -> Vec<String> {
    let mut found = vec![];
    for cap in def_re().captures_iter(code) {
        let name = cap[1].to_string();
        if SPIDER_METHODS.contains(&name.as_str()) && !found.contains(&name) {
            found.push(name);
        }
    }
    // 按 SPIDER_METHODS 的顺序排，输出稳定。
    let mut ordered: Vec<String> = SPIDER_METHODS
        .iter()
        .filter(|m| found.contains(&m.to_string()))
        .map(|m| m.to_string())
        .collect();
    ordered.dedup();
    ordered
}

/// 模块级（无缩进）`def 名(` 的方法名。
fn module_methods(code: &str) -> Vec<String> {
    let mut found = vec![];
    for cap in module_def_re().captures_iter(code) {
        let name = cap[1].to_string();
        if SPIDER_METHODS.contains(&name.as_str()) && !found.contains(&name) {
            found.push(name);
        }
    }
    found
}

/// `getName()` 方法里 `return "..."` 的字面量（静态命名用，不执行代码）。
pub fn static_get_name(code: &str) -> Option<String> {
    get_name_re()
        .captures(code)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 从导入来源（URL/文件路径）取文件名主干（去扩展名），作 Python 源名字。
/// URL 带查询串时先截掉；没有文件名时返回 None。
pub(crate) fn location_stem(location: &str) -> Option<String> {
    let path = location.split(['?', '#']).next().unwrap_or("");
    let file = path.rsplit(['/', '\\']).next().unwrap_or("");
    if file.is_empty() {
        return None;
    }
    let stem = match file.rfind('.') {
        Some(i) if i > 0 => &file[..i],
        _ => file,
    };
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_string())
    }
}

/// 把名字转成 id 后缀：只留小写字母数字与 `._-`，其余换 `-`，连续 `-` 合并，
/// 首尾去 `-`/`.`，超过 61 字符截断（id 全长 `py.` + 后缀 ≤ 64）。
pub(crate) fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 61 {
            break;
        }
    }
    let trimmed = out.trim_matches(|c| c == '-' || c == '.');
    if trimmed.is_empty() {
        "spider".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 看着像 Python 源码（行首有 def/class/import/from），但 `is_python`
/// 不认——通常是缺了蜘蛛面。导入检查用它把错误指向蜘蛛面，
/// 而不是报 JS 的通用错。
pub fn looks_like_python(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("def ")
            || line.starts_with("class ")
            || line.starts_with("import ")
            || line.starts_with("from ")
    })
}

/// 内容识别，不认扩展名：一段文本是 Python 蜘蛛，当且仅当
/// - 不是 JS 插件（`export const meta` 的文本一律按 JS 走），且
/// - 有 `class Spider(`，并至少带一个蜘蛛方法定义、`from base.spider import`
///   或 `BaseSpider` 出现；或
/// - 至少两个模块级 TVBox 函数定义（函数式蜘蛛）。
pub fn is_python(text: &str) -> bool {
    if text.contains("export const meta") {
        return false;
    }
    if class_re().is_match(text) {
        return !defined_methods(text).is_empty()
            || text.contains("from base.spider import")
            || text.contains("BaseSpider");
    }
    // 函数式：至少两个模块级 TVBox 函数，避免把单个 def 误认进来。
    let module = module_methods(text);
    module.iter().filter(|m| CONTENT_METHODS.contains(&m.as_str())).count() >= 2
}

/// 静态校验一段 Python 蜘蛛源码：确认写法与方法面。
/// 不执行代码；执行期的真实方法面以边车 /load 返回的为准。
pub fn scan_source(code: &str) -> Result<SpiderFace> {
    if !is_python(code) {
        return Err(PluginError::invalid(
            "不是可识别的 Python 蜘蛛：需要 class Spider 或模块级 TVBox 函数",
        ));
    }
    let class_style = class_re().is_match(code);
    let methods = if class_style {
        defined_methods(code)
    } else {
        module_methods(code)
    };
    if !methods
        .iter()
        .any(|m| CONTENT_METHODS.contains(&m.as_str()))
    {
        return Err(PluginError::invalid(
            "蜘蛛没有实现任何内容方法（homeContent / homeVideoContent / categoryContent / detailContent / searchContent / playerContent）",
        ));
    }
    Ok(SpiderFace {
        class_style,
        methods,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS_SPIDER: &str = r#"
try:
    from base.spider import BaseSpider
except ImportError:
    class BaseSpider: pass

class Spider(BaseSpider):
    def getName(self):
        return "dnyy"
    def homeContent(self, filter):
        return {"class": []}
    def categoryContent(self, tid, pg, filter, extend):
        return {"list": []}
    def detailContent(self, ids):
        return {"list": []}
    def searchContent(self, key, quick, pg="1"):
        return {"list": []}
    def playerContent(self, flag, id, vipFlags):
        return {"parse": 0, "url": id, "header": {}}
"#;

    const FUNCTION_SPIDER: &str = r#"
def homeContent(filter):
    return {"class": []}
def categoryContent(tid, pg, filter, extend):
    return {"list": []}
def detailContent(ids):
    return {"list": []}
def playerContent(flag, id, vipFlags):
    return {"parse": 0, "url": id, "header": {}}
"#;

    #[test]
    fn class_spider_detected() {
        assert!(is_python(CLASS_SPIDER));
        let face = scan_source(CLASS_SPIDER).unwrap();
        assert!(face.class_style);
        assert!(face.methods.contains(&"homeContent".to_string()));
        assert!(face.methods.contains(&"playerContent".to_string()));
        assert_eq!(static_get_name(CLASS_SPIDER).as_deref(), Some("dnyy"));
    }

    #[test]
    fn function_spider_detected() {
        assert!(is_python(FUNCTION_SPIDER));
        let face = scan_source(FUNCTION_SPIDER).unwrap();
        assert!(!face.class_style);
        assert!(face.methods.contains(&"detailContent".to_string()));
    }

    #[test]
    fn js_plugin_not_python() {
        let js = "export const meta = { id: \"x\", name: \"x\", version: \"1.0.0\", protocol: 1 };\n// class Spider( in a string\nconst s = \"def homeContent(\";\nexport default function (hg) { return {}; }\n";
        assert!(!is_python(js));
    }

    #[test]
    fn class_without_methods_rejected() {
        let code = "class Spider:\n    pass\n";
        assert!(!is_python(code));
        assert!(scan_source(code).is_err());
    }

    #[test]
    fn single_function_not_python() {
        let code = "def homeContent(filter):\n    return {}\n";
        assert!(!is_python(code));
    }

    #[test]
    fn html_not_python() {
        assert!(!is_python("<!doctype html><html><title>class Spider</title></html>"));
    }

    #[test]
    fn plain_python_not_spider() {
        assert!(!is_python("import os\nprint('hi')\n"));
    }
}
