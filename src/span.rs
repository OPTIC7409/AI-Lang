//! Source locations and the source map.

use std::rc::Rc;

/// A byte range inside a source file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: u32,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: u32, start: usize, end: usize) -> Span {
        Span { file, start: start as u32, end: end as u32 }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        if self.file != other.file {
            return self;
        }
        Span { file: self.file, start: self.start.min(other.start), end: self.end.max(other.end) }
    }

    pub fn len(self) -> usize {
        (self.end - self.start) as usize
    }

    pub fn is_empty(self) -> bool {
        self.start == self.end
    }
}

pub struct SourceFile {
    pub name: String,
    pub src: Rc<str>,
    line_starts: Vec<usize>,
}

impl SourceFile {
    pub fn new(name: String, src: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in src.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        SourceFile { name, src: Rc::from(src), line_starts }
    }

    /// 1-based line and 1-based column (counted in characters).
    pub fn line_col(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.src.len());
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line];
        let col = self.src[start..offset].chars().count() + 1;
        (line + 1, col)
    }

    /// The text of a 1-based line, without its trailing newline.
    pub fn line_text(&self, line: usize) -> &str {
        if line == 0 || line > self.line_starts.len() {
            return "";
        }
        let start = self.line_starts[line - 1];
        let end = if line < self.line_starts.len() { self.line_starts[line] } else { self.src.len() };
        self.src[start..end].trim_end_matches(['\n', '\r'])
    }

    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }
}

#[derive(Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> SourceMap {
        SourceMap { files: Vec::new() }
    }

    pub fn add(&mut self, name: impl Into<String>, src: impl Into<String>) -> u32 {
        self.files.push(SourceFile::new(name.into(), src.into()));
        (self.files.len() - 1) as u32
    }

    pub fn get(&self, id: u32) -> &SourceFile {
        &self.files[id as usize]
    }

    pub fn snippet(&self, span: Span) -> &str {
        let f = self.get(span.file);
        let s = (span.start as usize).min(f.src.len());
        let e = (span.end as usize).min(f.src.len()).max(s);
        &f.src[s..e]
    }

    /// "file.cog:3:7"
    pub fn location(&self, span: Span) -> String {
        let f = self.get(span.file);
        let (l, c) = f.line_col(span.start as usize);
        format!("{}:{}:{}", f.name, l, c)
    }
}
