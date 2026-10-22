use indexmap::IndexMap;

/// Returns the canonical representation of a name according to an IRC
/// case-mapping.
pub type CaseMapping = fn(&str) -> String;

fn map_bytes(name: &str, f: impl Fn(u8) -> u8) -> String {
    let bytes: Vec<u8> = name.bytes().map(f).collect();
    // Only ASCII bytes are rewritten to other ASCII bytes, so the result is
    // still valid UTF-8.
    String::from_utf8(bytes).expect("case-mapping preserves UTF-8")
}

pub fn casemap_ascii(name: &str) -> String {
    map_bytes(name, |b| b.to_ascii_lowercase())
}

pub fn casemap_rfc1459(name: &str) -> String {
    map_bytes(name, |b| match b {
        b'A'..=b'Z' => b.to_ascii_lowercase(),
        b'{' => b'[',
        b'}' => b']',
        b'\\' => b'|',
        b'~' => b'^',
        b => b,
    })
}

pub fn casemap_rfc1459_strict(name: &str) -> String {
    map_bytes(name, |b| match b {
        b'A'..=b'Z' => b.to_ascii_lowercase(),
        b'{' => b'[',
        b'}' => b']',
        b'\\' => b'|',
        b => b,
    })
}

pub const CASEMAPPING_ASCII: CaseMapping = casemap_ascii;
pub const CASEMAPPING_RFC1459: CaseMapping = casemap_rfc1459;
pub const CASEMAPPING_RFC1459_STRICT: CaseMapping = casemap_rfc1459_strict;

pub fn parse_casemapping(s: &str) -> Option<CaseMapping> {
    match s {
        "ascii" => Some(CASEMAPPING_ASCII),
        "rfc1459" => Some(CASEMAPPING_RFC1459),
        "rfc1459-strict" => Some(CASEMAPPING_RFC1459_STRICT),
        _ => None,
    }
}

/// A map whose keys are compared according to a case-mapping. The original
/// spelling of each key is preserved, as is insertion order.
///
/// Channel member lists are such maps: to keep them small, keys are stored
/// with their exact size, and the original spelling only when it differs
/// from the case-mapped key.
#[derive(Clone, Debug)]
pub struct CaseMappingMap<V> {
    m: IndexMap<Box<str>, Entry<V>>,
    casemap: CaseMapping,
}

#[derive(Clone, Debug)]
struct Entry<V> {
    /// The original spelling, if different from the case-mapped key
    name: Option<Box<str>>,
    value: V,
}

impl<V> Entry<V> {
    fn new(cm: &str, name: &str, value: V) -> Entry<V> {
        Entry {
            name: (cm != name).then(|| name.into()),
            value,
        }
    }
}

/// The original spelling of an entry's key.
fn original<'a, V>(cm: &'a str, e: &'a Entry<V>) -> &'a str {
    e.name.as_deref().unwrap_or(cm)
}

impl<V> CaseMappingMap<V> {
    pub fn new(casemap: CaseMapping) -> CaseMappingMap<V> {
        CaseMappingMap {
            m: IndexMap::new(),
            casemap,
        }
    }

    pub fn has(&self, name: &str) -> bool {
        self.m.contains_key(&*(self.casemap)(name))
    }

    pub fn len(&self) -> usize {
        self.m.len()
    }

    pub fn is_empty(&self) -> bool {
        self.m.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        self.m.get(&*(self.casemap)(name)).map(|e| &e.value)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut V> {
        self.m.get_mut(&*(self.casemap)(name)).map(|e| &mut e.value)
    }

    /// Sets a value. If the key already exists, its original spelling is
    /// kept.
    pub fn set(&mut self, name: &str, value: V) {
        let cm = (self.casemap)(name);
        if let Some(e) = self.m.get_mut(&*cm) {
            e.value = value;
        } else {
            let e = Entry::new(&cm, name, value);
            self.m.insert(cm.into_boxed_str(), e);
        }
    }

    pub fn del(&mut self, name: &str) -> Option<V> {
        self.m.shift_remove(&*(self.casemap)(name)).map(|e| e.value)
    }

    /// Iterates over (original key, value) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &V)> {
        self.m.iter().map(|(cm, e)| (original(cm, e), &e.value))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut V)> {
        self.m
            .iter_mut()
            .map(|(cm, e)| (e.name.as_deref().unwrap_or(cm), &mut e.value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.m.iter().map(|(cm, e)| original(cm, e))
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.m.values().map(|e| &e.value)
    }

    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.m.values_mut().map(|e| &mut e.value)
    }

    pub fn set_casemapping(&mut self, casemap: CaseMapping) {
        self.casemap = casemap;
        let old = std::mem::take(&mut self.m);
        for (cm, e) in old {
            let name = e.name.unwrap_or(cm);
            let cm = casemap(&name);
            let e = Entry::new(&cm, &name, e.value);
            self.m.insert(cm.into_boxed_str(), e);
        }
    }

    pub fn clear(&mut self) {
        self.m.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn casemaps() {
        assert_eq!(casemap_ascii("ABC{}~é"), "abc{}~é");
        assert_eq!(casemap_rfc1459("A{}\\~"), "a[]|^");
        assert_eq!(casemap_rfc1459_strict("A{}\\~"), "a[]|~");
    }

    #[test]
    fn map() {
        let mut m = CaseMappingMap::new(CASEMAPPING_RFC1459);
        m.set("Foo{", 1);
        assert!(m.has("foo["));
        m.set("FOO[", 2);
        assert_eq!(m.get("foo{"), Some(&2));
        // The original spelling is kept
        assert_eq!(m.keys().collect::<Vec<_>>(), ["Foo{"]);
        m.set_casemapping(CASEMAPPING_ASCII);
        assert!(!m.has("foo["));
        assert!(m.has("FOO{"));
        assert_eq!(m.keys().collect::<Vec<_>>(), ["Foo{"]);
        assert_eq!(m.del("foo{"), Some(2));
        assert!(m.is_empty());
    }

    #[test]
    fn map_spellings() {
        let mut m = CaseMappingMap::new(CASEMAPPING_ASCII);
        // Already case-mapped keys don't store a second spelling
        m.set("lower", 1);
        m.set("Mixed", 2);
        assert!(m.m["lower"].name.is_none());
        assert_eq!(m.m["mixed"].name.as_deref(), Some("Mixed"));
        assert_eq!(m.keys().collect::<Vec<_>>(), ["lower", "Mixed"]);
        assert_eq!(m.iter().collect::<Vec<_>>(), [("lower", &1), ("Mixed", &2)]);
        for (k, v) in m.iter_mut() {
            *v += k.len();
        }
        assert_eq!(m.values().collect::<Vec<_>>(), [&6, &7]);
        // A spelling which becomes the case-mapped key isn't stored again
        let mut m = CaseMappingMap::new(CASEMAPPING_ASCII);
        m.set("a[", 1);
        m.set_casemapping(CASEMAPPING_RFC1459);
        assert_eq!(m.keys().collect::<Vec<_>>(), ["a["]);
        assert!(m.m["a["].name.is_none());
        m.set("B{", 2);
        m.set_casemapping(CASEMAPPING_ASCII);
        assert_eq!(m.keys().collect::<Vec<_>>(), ["a[", "B{"]);
        assert_eq!(m.get("b{"), Some(&2));
    }
}
