use crate::{MAX_THEME_BYTES, ThemeDocument, ThemeTransferError};
use serde_json::{Map, Value};
fn item(
    value: &toml_edit::Item,
    depth: usize,
    budget: &mut usize,
) -> Result<Value, ThemeTransferError> {
    if depth > 8 || *budget == 0 {
        return Err(ThemeTransferError::TooLarge);
    }
    *budget -= 1;
    match value {
        toml_edit::Item::Value(value) => scalar(value, depth, budget),
        toml_edit::Item::Table(table) => {
            let mut out = Map::new();
            for (k, v) in table.iter() {
                out.insert(k.into(), item(v, depth + 1, budget)?);
            }
            Ok(Value::Object(out))
        }
        toml_edit::Item::ArrayOfTables(tables) => {
            let mut out = Vec::new();
            for table in tables.iter() {
                let mut object = Map::new();
                for (k, v) in table.iter() {
                    object.insert(k.into(), item(v, depth + 1, budget)?);
                }
                out.push(Value::Object(object));
            }
            Ok(Value::Array(out))
        }
        toml_edit::Item::None => Err(ThemeTransferError::InvalidDocument),
    }
}
fn scalar(
    value: &toml_edit::Value,
    depth: usize,
    budget: &mut usize,
) -> Result<Value, ThemeTransferError> {
    if depth > 8 || *budget == 0 {
        return Err(ThemeTransferError::TooLarge);
    }
    *budget -= 1;
    match value {
        toml_edit::Value::String(v) => Ok(Value::String(v.value().clone())),
        toml_edit::Value::Integer(v) => Ok(Value::Number((*v.value()).into())),
        toml_edit::Value::Array(array) => Ok(Value::Array(
            array
                .iter()
                .map(|v| scalar(v, depth + 1, budget))
                .collect::<Result<_, _>>()?,
        )),
        toml_edit::Value::InlineTable(table) => {
            let mut object = Map::new();
            for (k, v) in table.iter() {
                object.insert(k.into(), scalar(v, depth + 1, budget)?);
            }
            Ok(Value::Object(object))
        }
        _ => Err(ThemeTransferError::InvalidDocument),
    }
}
impl ThemeDocument {
    pub fn from_toml(bytes: &[u8]) -> Result<Self, ThemeTransferError> {
        if bytes.len() > MAX_THEME_BYTES {
            return Err(ThemeTransferError::TooLarge);
        }
        let text = std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map_err(|_| ThemeTransferError::InvalidDocument)?;
        let document: toml_edit::DocumentMut = text
            .parse()
            .map_err(|_| ThemeTransferError::InvalidDocument)?;
        let mut budget = 32 * 1024;
        let mut object = Map::new();
        for (k, v) in document.as_table().iter() {
            object.insert(k.into(), item(v, 0, &mut budget)?);
        }
        let bytes = serde_json::to_vec(&Value::Object(object))
            .map_err(|_| ThemeTransferError::InvalidDocument)?;
        Self::parse(&bytes)
    }
    pub fn to_toml(&self) -> Result<Vec<u8>, ThemeTransferError> {
        Self::parse(&serde_json::to_vec(self).map_err(|_| ThemeTransferError::InvalidDocument)?)?;
        let mut text = String::from("format = \"cshell.terminal-themes\"\nversion = 1\n");
        if self.themes.is_empty() {
            text.push_str("themes = []\n");
        }
        for theme in &self.themes {
            text.push_str(&format!(
                "\n[[themes]]\nname = \"{}\"\n[themes.palette]\n",
                theme.name
            ));
            for (key, color) in [
                ("foreground", theme.palette.foreground),
                ("background", theme.palette.background),
                ("cursor", theme.palette.cursor),
                ("selection", theme.palette.selection),
            ] {
                text.push_str(&format!(
                    "{key} = [{}, {}, {}]\n",
                    color[0], color[1], color[2]
                ));
            }
            text.push_str("ansi = [\n");
            for c in theme.palette.ansi {
                text.push_str(&format!("  [{}, {}, {}],\n", c[0], c[1], c[2]));
            }
            text.push_str("]\n");
        }
        if text.len() > MAX_THEME_BYTES {
            return Err(ThemeTransferError::TooLarge);
        }
        Ok(text.into_bytes())
    }
}
