//! 过滤规则 — 与官方语义一致：点文件、扩展名、大小上下限 + 内置系统忽略。

use crate::service::tasks::FilterRuleInput;
use regex::Regex;
use std::path::Path;

/// 官方内置忽略（字符串恢复）：Icon\r、sync_temp_dir、FNSYNC、.tmp、.temp、#开头、.download 临时目录
const BUILTIN_PATTERNS: &[&str] = &[
    r"^Icon\r$",
    r".*sync_temp_dir.*",
    r".*FNSYNC.*",
    r".*\.tmp$",
    r".*\.temp$",
    r"^#.*",
    r"^\.download($|/)",
];

#[derive(Clone)]
pub struct FileFilter {
    excluded_paths: Vec<String>,
    ignore_dotfiles: bool,
    ignore_ext: Option<Vec<String>>,
    size_lower: Option<u64>,
    size_upper: Option<u64>,
    builtins: Vec<Regex>,
}

impl FileFilter {
    pub fn from_rule(rule: &FilterRuleInput) -> Self {
        let ignore_ext = if rule.enable_ignore_ext == 1 {
            Some(
                rule.ignore_extensions
                    .iter()
                    .map(|s| s.trim().to_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        } else {
            None
        };
        Self {
            excluded_paths: Vec::new(),
            ignore_dotfiles: rule.ignore_dotfiles == 1,
            size_lower: if rule.enable_size_lower == 1 {
                rule.size_lower_bound.map(|v| v.max(0) as u64)
            } else {
                None
            },
            size_upper: if rule.enable_size_upper == 1 {
                rule.size_upper_bound.map(|v| v.max(0) as u64)
            } else {
                None
            },
            ignore_ext,
            builtins: BUILTIN_PATTERNS
                .iter()
                .map(|p| Regex::new(p).expect("builtin regex"))
                .collect(),
        }
    }

    pub fn from_db(rule: Option<&FilterRuleInput>) -> Self {
        match rule {
            Some(r) => Self::from_rule(r),
            None => Self::from_rule(&FilterRuleInput::default_rule()),
        }
    }

    pub fn with_excluded_paths(mut self, paths: Vec<String>) -> Self {
        self.excluded_paths = paths
            .into_iter()
            .map(|p| p.trim_matches('/').to_string())
            .collect();
        self
    }

    /// 相对路径（/ 分隔）+ 可选文件大小。
    pub fn should_sync(&self, rel_path: &str, size: Option<u64>) -> bool {
        if self.excluded_paths.iter().any(|excluded| {
            excluded.is_empty()
                || rel_path == excluded
                || rel_path.starts_with(&format!("{excluded}/"))
        }) {
            return false;
        }
        let p = Path::new(rel_path);
        for comp in p.components() {
            let name = comp.as_os_str().to_string_lossy();
            // 内置忽略对每级目录名生效
            if self.builtins.iter().any(|re| re.is_match(&name)) {
                return false;
            }
            // 点文件/目录
            if self.ignore_dotfiles && name.starts_with('.') && name != "." && name != "/" {
                return false;
            }
        }
        if let (Some(exts), Some(actual)) = (&self.ignore_ext, p.extension()) {
            let e = actual.to_string_lossy().to_lowercase();
            if exts.iter().any(|x| x == &e) {
                return false;
            }
        }
        if let Some(sz) = size {
            if let Some(lo) = self.size_lower {
                if sz < lo {
                    return false;
                }
            }
            if let Some(hi) = self.size_upper {
                if sz > hi {
                    return false;
                }
            }
        }
        true
    }

    /// 目录是否应被 watch（目录名过滤）。
    pub fn should_watch_dir(&self, rel_path: &str) -> bool {
        self.should_sync(rel_path, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> FilterRuleInput {
        FilterRuleInput {
            ignore_dotfiles: 1,
            enable_ignore_ext: 1,
            ignore_extensions: vec!["tmp".into(), "log".into()],
            enable_size_lower: 1,
            size_lower_bound: Some(10),
            enable_size_upper: 1,
            size_upper_bound: Some(1000),
        }
    }

    #[test]
    fn dotfiles_ignored() {
        let f = FileFilter::from_rule(&rule());
        assert!(!f.should_sync(".hidden", None));
        assert!(f.should_sync("visible.txt", None));
        assert!(!f.should_sync("dir/.git/config", None));
    }

    #[test]
    fn extensions_ignored() {
        let f = FileFilter::from_rule(&rule());
        assert!(!f.should_sync("debug.TMP", Some(100)));
        assert!(!f.should_sync("app.LOG", Some(100)));
        assert!(f.should_sync("photo.jpg", Some(100)));
    }

    #[test]
    fn size_bounds() {
        let f = FileFilter::from_rule(&rule());
        assert!(!f.should_sync("small.txt", Some(5)));
        assert!(f.should_sync("ok.txt", Some(100)));
        assert!(!f.should_sync("big.txt", Some(2000)));
    }

    #[test]
    fn builtin_patterns() {
        let f = FileFilter::from_rule(&rule());
        assert!(!f.should_sync("FNSYNC/backup", None));
        assert!(!f.should_sync("cache.tmp", Some(100)));
        assert!(!f.should_sync("x/y.temp", None));
        assert!(!f.should_sync("sync_temp_dir/junk", None));
        assert!(!f.should_sync(".download/partial", None));
    }

    #[test]
    fn disabled_rules_pass() {
        let r = FilterRuleInput {
            ignore_dotfiles: 0,
            enable_ignore_ext: 0,
            ignore_extensions: vec![],
            enable_size_lower: 0,
            size_lower_bound: None,
            enable_size_upper: 0,
            size_upper_bound: None,
        };
        let f = FileFilter::from_rule(&r);
        assert!(f.should_sync(".hidden", Some(1)));
        assert!(!f.should_sync("a.tmp", Some(5))); // 内置 .tmp 规则始终生效
    }
}

#[cfg(test)]
mod selective_tests {
    #[test]
    fn exclusions_use_directory_boundaries_and_support_excluding_root() {
        let filter = super::FileFilter::from_db(None).with_excluded_paths(vec!["excluded".into()]);
        assert!(!filter.should_sync("excluded/file.txt", Some(10)));
        assert!(!filter.should_sync("excluded", None));
        assert!(filter.should_sync("excluded-other/file.txt", Some(10)));
        let root = super::FileFilter::from_db(None).with_excluded_paths(vec!["".into()]);
        assert!(!root.should_sync("anything.txt", Some(1)));
    }
}
