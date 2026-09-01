//! config set：基于 toml_edit 的键值写回，保留文件中的注释与格式（决议 D113）。

use anyhow::{Context, bail};
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table, Value};

/// 支持的顶层配置段（用于报错提示）。
pub const KNOWN_SECTIONS: &[&str] = &["model", "budget", "safety", "search", "agent"];

/// 把字符串值按直觉类型转换：true/false → 布尔，整数 / 浮点 → 数值，其余 → 字符串。
fn coerce(raw: &str) -> Value {
    match raw {
        "true" => return Value::from(true),
        "false" => return Value::from(false),
        _ => {}
    }
    if let Ok(int) = raw.parse::<i64>() {
        return Value::from(int);
    }
    if let Ok(float) = raw.parse::<f64>() {
        return Value::from(float);
    }
    Value::from(raw)
}

/// 设置 `dotted.key = value` 并写回文件。文件不存在时先写默认模板。
///
/// 价格表 `[[prices]]` 是数组结构，刻意不支持 set，提示手编。
pub fn set_key(config_path: &Path, full_key: &str, raw_value: &str) -> anyhow::Result<()> {
    if full_key.starts_with("prices") {
        bail!(
            "价格表是数组结构（[[prices]]），请直接编辑 {} 手工修改",
            config_path.display()
        );
    }
    if !config_path.exists() {
        let template = crate::config::AppConfig::template("", "");
        std::fs::write(config_path, template)
            .with_context(|| format!("初始化配置文件失败：{}", config_path.display()))?;
    }
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("读取配置文件失败：{}", config_path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("解析配置文件失败：{}", config_path.display()))?;

    let parts: Vec<&str> = full_key.split('.').collect();
    if parts.len() < 2 {
        bail!(
            "键名需要形如 model.endpoint（可用的顶层段：{}）",
            KNOWN_SECTIONS.join(" / ")
        );
    }
    let mut table = doc.as_table_mut();
    for part in &parts[..parts.len() - 1] {
        if !table.contains_key(part) {
            table.insert(part, Item::Table(Table::new()));
        }
        table = match table.get_mut(part) {
            Some(Item::Table(t)) => t,
            Some(Item::Value(_)) => bail!("“{part}”是标量值，不能继续下钻"),
            _ => bail!("“{part}”不是可修改的配置表"),
        };
    }
    let last = *parts.last().expect("split 至少两段");
    table.insert(last, Item::Value(coerce(raw_value)));

    std::fs::write(config_path, doc.to_string())
        .with_context(|| format!("写回配置文件失败：{}", config_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_preserves_comments_and_coerces_types() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "# 顶部注释\n[model]\nendpoint = \"https://a.example\"\nmodel = \"m1\" # 行内注释\ncontext_len = 1000\n\n[budget]\nlimit_cny = 10.0\n",
        )
        .unwrap();

        set_key(&path, "model.context_len", "64000").unwrap();
        set_key(&path, "model.thinking", "true").unwrap();
        set_key(&path, "budget.limit_cny", "5.5").unwrap();
        set_key(&path, "model.endpoint", "https://b.example").unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# 顶部注释"));
        assert!(text.contains("# 行内注释"));
        let config: crate::config::AppConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.model.context_len, 64000);
        assert!(config.model.thinking);
        assert_eq!(config.budget.limit_cny, 5.5);
        assert_eq!(config.model.endpoint, "https://b.example");
    }

    #[test]
    fn rejects_bad_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, crate::config::AppConfig::template("", "")).unwrap();
        // 缺少表前缀
        assert!(set_key(&path, "endpoint", "x").is_err());
        // 价格表不支持 set
        assert!(set_key(&path, "prices.model", "x").is_err());
        // 未知段允许创建（宽容策略）
        assert!(set_key(&path, "custom_section.key", "x").is_ok());
    }
}
