// a simple test
#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    #[test]
    fn test_hashmap() {
        let mut map = HashMap::new();
        map.insert("a", 1);
        map.insert("b", 2);
        map.insert("c", 3);

        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(map.get("b"), Some(&2));
        assert_eq!(map.get("c"), Some(&3));
    }
}
