//! You can run this test suite with:
//!
//!     cargo test --test witxt
//!
//! An argument can be passed as well to filter, based on filename, which test
//! to run
//!
//!     cargo test --test witxt foo.witxt

use anyhow::{anyhow, bail, Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use wast::parser::{self, Parse, ParseBuffer, Parser};
use witx::Representable;

fn main() {
    let tests = find_tests();
    let filter = std::env::args().nth(1);

    let tests = tests
        .par_iter()
        .filter_map(|test| {
            if let Some(filter) = &filter {
                if let Some(s) = test.to_str() {
                    if !s.contains(filter) {
                        return None;
                    }
                }
            }
            let contents = std::fs::read(test).unwrap();
            Some((test, contents))
        })
        .collect::<Vec<_>>();

    println!("running {} test files\n", tests.len());

    let ntests = AtomicUsize::new(0);
    let errors = tests
        .par_iter()
        .filter_map(|(test, contents)| {
            WitxtRunner {
                ntests: &ntests,
                documents: HashMap::new(),
            }
            .run(test, contents)
            .err()
        })
        .collect::<Vec<_>>();

    if !errors.is_empty() {
        for msg in errors.iter() {
            eprintln!("{:?}", msg);
        }

        panic!("{} tests failed", errors.len())
    }

    println!(
        "test result: ok. {} directives passed\n",
        ntests.load(SeqCst)
    );
}

/// Recursively finds all tests in a whitelisted set of directories which we
/// then load up and test in parallel.
fn find_tests() -> Vec<PathBuf> {
    let mut tests = Vec::new();
    find_tests("tests/witxt".as_ref(), &mut tests);
    tests.sort();
    return tests;

    fn find_tests(path: &Path, tests: &mut Vec<PathBuf>) {
        for f in path.read_dir().unwrap() {
            let f = f.unwrap();
            if f.file_type().unwrap().is_dir() {
                find_tests(&f.path(), tests);
                continue;
            }

            match f.path().extension().and_then(|s| s.to_str()) {
                Some("witxt") => {}
                _ => continue,
            }
            tests.push(f.path());
        }
    }
}

struct WitxtRunner<'a> {
    ntests: &'a AtomicUsize,
    documents: HashMap<String, witx::Document>,
}

impl WitxtRunner<'_> {
    fn run(&mut self, test: &Path, contents: &[u8]) -> Result<()> {
        let contents = str::from_utf8(contents)?;
        macro_rules! adjust {
            ($e:expr) => {{
                let mut e = wast::Error::from($e);
                e.set_path(test);
                e.set_text(contents);
                e
            }};
        }
        let buf = ParseBuffer::new(contents).map_err(|e| adjust!(e))?;
        let witxt = parser::parse::<Witxt>(&buf).map_err(|e| adjust!(e))?;

        let errors = witxt
            .directives
            .into_iter()
            .filter_map(|directive| {
                let (line, col) = directive.span().linecol_in(contents);
                self.test_directive(contents, test, directive)
                    .with_context(|| {
                        format!(
                            "failed directive on {}:{}:{}",
                            test.display(),
                            line + 1,
                            col + 1
                        )
                    })
                    .err()
            })
            .collect::<Vec<_>>();
        if errors.is_empty() {
            return Ok(());
        }
        let mut s = format!("{} test failures in {}:", errors.len(), test.display());
        for mut error in errors {
            if let Some(err) = error.downcast_mut::<wast::Error>() {
                err.set_path(test);
                err.set_text(contents);
            }
            s.push_str("\n\n\t--------------------------------\n\n\t");
            s.push_str(&format!("{:?}", error).replace("\n", "\n\t"));
        }
        bail!("{}", s)
    }

    fn test_directive(
        &mut self,
        contents: &str,
        test: &Path,
        directive: WitxtDirective,
    ) -> Result<()> {
        match directive {
            WitxtDirective::Witx(witx) => {
                let doc = witx.document(contents, test)?;
                self.bump_ntests();
                if let Some(name) = witx.id {
                    self.documents.insert(name.name().to_string(), doc);
                }
            }
            WitxtDirective::AssertInvalid { witx, message, .. } => {
                let err = match witx.document(contents, test) {
                    Ok(_) => bail!("witx was valid when it shouldn't be"),
                    Err(e) => format!("{:?}", anyhow::Error::from(e)),
                };
                if !err.contains(message) {
                    bail!("expected error {:?}\nfound error {}", message, err);
                }
                self.bump_ntests();
            }
            WitxtDirective::AssertRepEquality { repr, t1, t2, .. } => {
                let (t1m, t1t) = t1;
                let (t2m, t2t) = t2;
                let t1d = self
                    .documents
                    .get(t1m.name())
                    .ok_or_else(|| anyhow!("no document named {:?}", t1m.name()))?;
                let t2d = self
                    .documents
                    .get(t2m.name())
                    .ok_or_else(|| anyhow!("no document named {:?}", t2m.name()))?;
                let t1 = t1d
                    .typename(&witx::Id::new(t1t))
                    .ok_or_else(|| anyhow!("no document named {:?}", t1t))?;
                let t2 = t2d
                    .typename(&witx::Id::new(t2t))
                    .ok_or_else(|| anyhow!("no document named {:?}", t2t))?;
                match (repr, t1.type_().representable(&t2.type_())) {
                    (RepEquality::Eq, witx::RepEquality::Eq)
                    | (RepEquality::NotEq, witx::RepEquality::NotEq) => {}
                    (a, b) => {
                        bail!("expected {:?} representation, got {:?}", a, b);
                    }
                }
                self.bump_ntests();
            }
        }
        Ok(())
    }

    fn bump_ntests(&self) {
        self.ntests.fetch_add(1, SeqCst);
    }
}

mod kw {
    wast::custom_keyword!(assert_invalid);
    wast::custom_keyword!(assert_representable);
    wast::custom_keyword!(witx);
    wast::custom_keyword!(eq);
    wast::custom_keyword!(noteq);
}

struct Witxt<'a> {
    directives: Vec<WitxtDirective<'a>>,
}

impl<'a> Parse<'a> for Witxt<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let mut directives = Vec::new();
        while !parser.is_empty() {
            directives.push(parser.parens(|p| p.parse())?);
        }
        Ok(Witxt { directives })
    }
}

enum WitxtDirective<'a> {
    Witx(Witx<'a>),
    AssertInvalid {
        span: wast::Span,
        witx: Witx<'a>,
        message: &'a str,
    },
    AssertRepEquality {
        span: wast::Span,
        repr: RepEquality,
        t1: (wast::Id<'a>, &'a str),
        t2: (wast::Id<'a>, &'a str),
    },
}

impl WitxtDirective<'_> {
    fn span(&self) -> wast::Span {
        match self {
            WitxtDirective::Witx(w) => w.span,
            WitxtDirective::AssertInvalid { span, .. } => *span,
            WitxtDirective::AssertRepEquality { span, .. } => *span,
        }
    }
}

impl<'a> Parse<'a> for WitxtDirective<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let mut l = parser.lookahead1();
        if l.peek::<kw::witx>() {
            Ok(WitxtDirective::Witx(parser.parse()?))
        } else if l.peek::<kw::assert_invalid>() {
            let span = parser.parse::<kw::assert_invalid>()?.0;
            Ok(WitxtDirective::AssertInvalid {
                span,
                witx: parser.parens(|p| p.parse())?,
                message: parser.parse()?,
            })
        } else if l.peek::<kw::assert_representable>() {
            let span = parser.parse::<kw::assert_representable>()?.0;
            Ok(WitxtDirective::AssertRepEquality {
                span,
                repr: parser.parse()?,
                t1: (parser.parse()?, parser.parse()?),
                t2: (parser.parse()?, parser.parse()?),
            })
        } else {
            Err(l.error())
        }
    }
}

struct Witx<'a> {
    span: wast::Span,
    id: Option<wast::Id<'a>>,
    decls: Vec<witx::parser::Documented<'a, witx::parser::DeclSyntax<'a>>>,
}

impl Witx<'_> {
    fn document(&self, contents: &str, file: &Path) -> Result<witx::Document> {
        let mut validator = witx::DocValidation::new();
        let mut definitions = Vec::new();
        for decl in self.decls.iter() {
            let def = validator
                .scope(contents, file)
                .validate_decl(&decl.item, &decl.comments)
                .map_err(witx::WitxError::Validation)?;
            definitions.push(def);
        }
        Ok(validator.into_document(definitions))
    }
}

impl<'a> Parse<'a> for Witx<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let span = parser.parse::<kw::witx>()?.0;
        let id = parser.parse()?;
        let mut decls = Vec::new();
        while !parser.is_empty() {
            decls.push(parser.parens(|p| p.parse())?);
        }
        Ok(Witx { id, span, decls })
    }
}

#[derive(Debug)]
enum RepEquality {
    Eq,
    NotEq,
}

impl<'a> Parse<'a> for RepEquality {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let mut l = parser.lookahead1();
        if l.peek::<kw::eq>() {
            parser.parse::<kw::eq>()?;
            Ok(RepEquality::Eq)
        } else if l.peek::<kw::noteq>() {
            parser.parse::<kw::noteq>()?;
            Ok(RepEquality::NotEq)
        } else {
            Err(l.error())
        }
    }
}
