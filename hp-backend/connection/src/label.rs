//! Метка набора дыр в логах. Роутер держит два набора (к телефону и к серверу),
//! и без метки их сообщения не отличить. Пустая метка ничего не печатает, так
//! что у одиночного `peer` логи выглядят как раньше.

use std::fmt;
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct Label(Arc<str>);

impl Label {
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            write!(f, "[{}] ", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_label_prints_nothing_named_label_gets_brackets() {
        assert_eq!(Label::default().to_string(), "");
        assert_eq!(Label::new("").to_string(), "");
        assert_eq!(Label::new("phone").to_string(), "[phone] ");
    }
}
