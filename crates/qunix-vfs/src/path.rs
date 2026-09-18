//! Splitting a path into what a walker visits, and nothing else.
//!
//! No I/O and no opinion about what the components mean: `..` at a mountpoint
//! and `..` at the root resolve differently, and only the walker knows which
//! it is looking at.

use qunix_abi::{NAME_MAX, PATH_MAX};

/// One step of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component<'a> {
    /// `.`
    Current,
    /// `..`
    Parent,
    /// Anything else.
    Name(&'a str),
}

/// Why a path could not be decomposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    /// The empty string. Refused rather than read as `.`, so a path built by
    /// concatenation that came out empty is an error at the call rather than
    /// the working directory returned successfully.
    Empty,
    /// Longer than `PATH_MAX`.
    TooLong,
    /// One component is longer than `NAME_MAX`, which is the array a
    /// `DirEntry` copies it into.
    ComponentTooLong,
}

/// A validated path, borrowed from the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Path<'a> {
    raw: &'a str,
    absolute: bool,
}

impl<'a> Path<'a> {
    /// Validates `raw` and records whether it is absolute.
    ///
    /// Both bounds are checked here rather than while walking, so a path that
    /// cannot be represented is refused before any filesystem is touched.
    pub fn parse(raw: &'a str) -> Result<Self, PathError> {
        if raw.is_empty() {
            return Err(PathError::Empty);
        }
        if raw.len() > PATH_MAX {
            return Err(PathError::TooLong);
        }
        if raw.split('/').any(|component| component.len() > NAME_MAX) {
            return Err(PathError::ComponentTooLong);
        }
        Ok(Self { raw, absolute: raw.starts_with('/') })
    }

    pub fn is_absolute(&self) -> bool {
        self.absolute
    }

    /// The components a walker visits, in order.
    ///
    /// Empty segments are dropped, which is what collapses `//a///b/` to
    /// `a`, `b`. A walker that received the empty ones would look up `""` in a
    /// directory: at best a failure for the wrong reason, at worst a match
    /// against an entry whose name really is empty.
    pub fn components(&self) -> impl Iterator<Item = Component<'a>> + '_ {
        self.raw.split('/').filter(|s| !s.is_empty()).map(|s| match s {
            "." => Component::Current,
            ".." => Component::Parent,
            name => Component::Name(name),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_decomposes_into_the_components_a_walker_visits() {
        let path = Path::parse("/usr/local/bin").expect("a plain absolute path was refused");
        assert!(path.is_absolute());
        let names: Vec<&str> = path
            .components()
            .map(|c| match c {
                Component::Name(n) => n,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(names, ["usr", "local", "bin"]);
    }

    #[test]
    fn repeated_and_trailing_separators_produce_no_components() {
        // `//a///b/` is the same path as `/a/b`. A walker that saw empty
        // components between them would look up "" in a directory, which either
        // fails for the wrong reason or, worse, matches an entry with an empty
        // name on a filesystem that permits one.
        let path = Path::parse("//a///b/").unwrap();
        let names: Vec<&str> = path
            .components()
            .map(|c| match c {
                Component::Name(n) => n,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn dot_and_dotdot_are_components_rather_than_names() {
        // Resolved by the walker, not here: `..` at a mountpoint means something
        // this crate cannot know. Turning them into `Name("..")` would make the
        // walker look for a directory entry called `..`, which is how a filesystem
        // that does not store one becomes unable to leave a directory.
        let path = Path::parse("a/./../b").unwrap();
        let got: Vec<Component> = path.components().collect();
        assert_eq!(
            got,
            [Component::Name("a"), Component::Current, Component::Parent, Component::Name("b")]
        );
    }

    #[test]
    fn a_relative_path_says_so() {
        assert!(!Path::parse("a/b").unwrap().is_absolute());
        assert!(Path::parse("/").unwrap().is_absolute());
    }

    #[test]
    fn the_root_has_no_components() {
        let root = Path::parse("/").unwrap();
        assert!(root.is_absolute());
        assert_eq!(root.components().count(), 0, "root resolved to a component to look up");
    }

    #[test]
    fn an_empty_path_is_refused_rather_than_resolved_as_the_current_directory() {
        // The refusal that matters. Treating "" as "." makes `open("")` return the
        // working directory, and every caller that built a path by concatenation
        // and got it wrong receives a directory instead of an error.
        assert_eq!(Path::parse(""), Err(PathError::Empty));
    }

    #[test]
    fn a_path_longer_than_the_bound_is_refused() {
        let long = "/".to_string() + &"a".repeat(PATH_MAX);
        assert_eq!(Path::parse(&long), Err(PathError::TooLong));
    }

    #[test]
    fn a_component_longer_than_the_bound_is_refused_even_in_a_short_path() {
        // Separate from the whole-path bound: a single component is copied into a
        // fixed `[u8; NAME_MAX]` in `DirEntry`, so a path well inside `PATH_MAX`
        // can still overrun that array.
        let name = "b".repeat(NAME_MAX + 1);
        assert_eq!(Path::parse(&format!("/{name}")), Err(PathError::ComponentTooLong));
        // And exactly at the bound is accepted, so the check is not off by one.
        let exact = "b".repeat(NAME_MAX);
        assert!(Path::parse(&format!("/{exact}")).is_ok());
    }
}
