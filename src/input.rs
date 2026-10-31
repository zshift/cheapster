//! Parsing the items the user wants to make.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wanted {
    Id(u32, u32),
    Name(String, u32),
}

/// Parses one entry such as `Iron Ingot`, `3x Iron Ingot`, `Iron Ingot x3`, `Iron Ingot:3`
/// or a numeric item id (`5057`, `5057:3`).
pub fn parse_entry(entry: &str) -> Option<Wanted> {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') {
        return None;
    }
    let (name, qty) = split_quantity(entry);
    Some(match name.parse::<u32>() {
        Ok(id) => Wanted::Id(id, qty),
        Err(_) => Wanted::Name(name.to_string(), qty),
    })
}

fn split_quantity(entry: &str) -> (&str, u32) {
    if let Some((name, qty)) = entry.rsplit_once(':')
        && let Ok(q) = qty.trim().parse()
    {
        return (name.trim(), q);
    }
    if let Some((first, rest)) = entry.split_once(char::is_whitespace) {
        let first = first.strip_suffix(['x', 'X']).unwrap_or(first);
        if let Ok(q) = first.parse() {
            return (rest.trim(), q);
        }
    }
    if let Some((rest, last)) = entry.rsplit_once(char::is_whitespace) {
        let last = last.strip_prefix(['x', 'X']).unwrap_or(last);
        if let Ok(q) = last.parse() {
            return (rest.trim(), q);
        }
    }
    (entry, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(n: &str, q: u32) -> Option<Wanted> {
        Some(Wanted::Name(n.to_string(), q))
    }

    #[test]
    fn parses_quantities() {
        assert_eq!(parse_entry("Iron Ingot"), name("Iron Ingot", 1));
        assert_eq!(parse_entry("3x Iron Ingot"), name("Iron Ingot", 3));
        assert_eq!(parse_entry("3 Iron Ingot"), name("Iron Ingot", 3));
        assert_eq!(parse_entry("Iron Ingot x3"), name("Iron Ingot", 3));
        assert_eq!(parse_entry("Iron Ingot:3"), name("Iron Ingot", 3));
        assert_eq!(parse_entry("5057:2"), Some(Wanted::Id(5057, 2)));
        assert_eq!(parse_entry("5057"), Some(Wanted::Id(5057, 1)));
        assert_eq!(parse_entry("Grade 8 Tincture of Strength"), name("Grade 8 Tincture of Strength", 1));
        assert_eq!(parse_entry("# comment"), None);
    }
}
