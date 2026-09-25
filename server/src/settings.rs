//! Настройки сервера: переменные окружения и, необязательно, файл `KEY=VALUE`.
//! У службы Windows своего окружения нет, поэтому настройки лежат в файле
//! (`server.env`, тот же формат, что у `.env` для `run.sh`); переменная окружения,
//! если задана, главнее файла. Относительные пути в файле считаются от его каталога.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct Settings {
    file: HashMap<String, String>,
    base_dir: PathBuf,
}

impl Settings {
    pub fn load(config: Option<&Path>) -> Result<Self> {
        let Some(path) = config else {
            return Ok(Self { file: HashMap::new(), base_dir: PathBuf::new() });
        };
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("не удалось прочитать файл настроек {}", path.display()))?;
        let base_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Ok(Self { file: parse(&text).with_context(|| path.display().to_string())?, base_dir })
    }

    /// Значение: переменная окружения главнее файла.
    pub fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().or_else(|| self.file.get(name).cloned())
    }

    /// Путь из настроек: относительный считается от каталога файла настроек.
    pub fn resolve(&self, value: &str) -> PathBuf {
        self.base_dir.join(value)
    }
}

/// `KEY=VALUE` построчно; пустые строки и `# комментарии` пропускаются, значение
/// может быть в одинарных или двойных кавычках, перед ключом допустим `export `.
fn parse(text: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for (number, raw) in text.lines().enumerate() {
        let line = raw.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let (key, value) = line
            .split_once('=')
            .with_context(|| format!("строка {}: ожидается KEY=VALUE", number + 1))?;
        let value = value.trim();
        let value = ["\"", "'"]
            .iter()
            .find_map(|q| value.strip_prefix(q)?.strip_suffix(q))
            .unwrap_or(value);
        map.insert(key.trim().to_string(), value.to_string());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_keys_comments_quotes_and_export() {
        let map = parse("# заголовок\n\nA=1\nexport B = два \nC=\"с пробелом\"\nD='x'\nE=\n").unwrap();
        assert_eq!(map["A"], "1");
        assert_eq!(map["B"], "два");
        assert_eq!(map["C"], "с пробелом");
        assert_eq!(map["D"], "x");
        assert_eq!(map["E"], "");
    }

    #[test]
    fn bom_and_windows_line_endings_are_tolerated() {
        let map = parse("\u{feff}A=1\r\nB=2\r\n").unwrap();
        assert_eq!((map["A"].as_str(), map["B"].as_str()), ("1", "2"));
    }

    #[test]
    fn line_without_equals_is_an_error_with_its_number() {
        let error = parse("A=1\nмусор\n").unwrap_err().to_string();
        assert!(error.contains("строка 2"), "{error}");
    }

    #[test]
    fn relative_paths_resolve_against_the_config_directory() {
        let dir = std::env::temp_dir().join(format!("hp-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("server.env");
        std::fs::write(&file, "HP_SETTINGS_TEST_KEY=value\n").unwrap();
        let settings = Settings::load(Some(&file)).unwrap();
        assert_eq!(settings.get("HP_SETTINGS_TEST_KEY").as_deref(), Some("value"));
        assert_eq!(settings.resolve("ca.crt"), dir.join("ca.crt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn without_a_file_paths_stay_relative_to_the_working_directory() {
        let settings = Settings::load(None).unwrap();
        assert_eq!(settings.resolve("../ca.crt"), PathBuf::from("../ca.crt"));
    }
}
