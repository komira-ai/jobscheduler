//! Placeholder strings (format section 1.3): text with `${tree:NAME}` and
//! `${developer_dir}` and nothing else that expands.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{ExpandError, TemplateError};
use crate::names::TreeName;
use crate::validate::MAX_VALUE_BYTES;

/// One piece of a [`Template`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Text that is used as written.
    Lit(String),
    /// `${tree:NAME}`: the directory of a tree of the bundle.
    Tree(TreeName),
    /// `${developer_dir}`: the selected host Xcode's developer directory.
    DeveloperDir,
}

/// A string with placeholders, parsed and normalised.
///
/// The rules, which are the same for a bundle config's values, a job's environment
/// values and a job's arguments:
///
/// - at most [`MAX_VALUE_BYTES`] bytes, no NUL and no line break;
/// - the placeholders are `${tree:NAME}` (a valid tree name) and `${developer_dir}`;
///   any other `${` is refused;
/// - what follows a placeholder is empty or starts with `/` (so a placeholder is a
///   whole path component and a path continues below it), and two placeholders are
///   never adjacent;
/// - in a path (the text after a placeholder, and a literal that starts with `/`) no
///   component is empty, `.` or `..`. This is the normalised-result rule: since
///   `${tree:NAME}` expands to a normalised absolute path, a value that passes cannot
///   name a place outside that tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Template {
    text: String,
    segments: Vec<Segment>,
}

/// Checks the components of a path, the part after its leading `/`.
pub(crate) fn check_components(rest: &str) -> Result<(), TemplateError> {
    for component in rest.split('/') {
        match component {
            "" => return Err(TemplateError::EmptyComponent),
            "." => return Err(TemplateError::CurrentDir),
            ".." => return Err(TemplateError::ParentDir),
            _ => {}
        }
    }
    Ok(())
}

fn split(text: &str) -> Result<Vec<Segment>, TemplateError> {
    let mut segments = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        if start > 0 {
            segments.push(Segment::Lit(rest[..start].to_string()));
        }
        let after = &rest[start + 2..];
        let end = after.find('}').ok_or(TemplateError::Unterminated)?;
        let body = &after[..end];
        let segment = match (body, body.strip_prefix("tree:")) {
            ("developer_dir", _) => Segment::DeveloperDir,
            (_, Some(name)) => Segment::Tree(
                TreeName::parse(name).map_err(|_| TemplateError::BadTreeName(name.to_string()))?,
            ),
            (_, None) => return Err(TemplateError::Unknown(body.to_string())),
        };
        segments.push(segment);
        rest = &after[end + 1..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Lit(rest.to_string()));
    }
    Ok(segments)
}

fn normalise(segments: &[Segment]) -> Result<(), TemplateError> {
    let mut after_placeholder = false;
    for segment in segments {
        match segment {
            Segment::Lit(lit) => {
                match (lit.strip_prefix('/'), after_placeholder) {
                    (Some(rest), _) => check_components(rest)?,
                    (None, true) => return Err(TemplateError::Attached),
                    (None, false) => {}
                }
                after_placeholder = false;
            }
            Segment::Tree(_) | Segment::DeveloperDir => {
                if after_placeholder {
                    return Err(TemplateError::Attached);
                }
                after_placeholder = true;
            }
        }
    }
    Ok(())
}

impl Template {
    /// Parses and normalises a placeholder string.
    pub fn parse(text: &str) -> Result<Template, TemplateError> {
        if text.len() > MAX_VALUE_BYTES {
            return Err(TemplateError::TooLong {
                max: MAX_VALUE_BYTES,
            });
        }
        if text.contains('\0') {
            return Err(TemplateError::Nul);
        }
        if text.contains(['\n', '\r']) {
            return Err(TemplateError::Newline);
        }
        let segments = split(text)?;
        normalise(&segments)?;
        Ok(Template {
            text: text.to_string(),
            segments,
        })
    }

    /// The text as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The pieces, in order.
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    /// The trees the text names, in order, with repeats.
    pub fn trees(&self) -> impl Iterator<Item = &TreeName> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Tree(name) => Some(name),
            _ => None,
        })
    }

    /// Whether the text uses `${developer_dir}`.
    pub fn uses_developer_dir(&self) -> bool {
        self.segments.contains(&Segment::DeveloperDir)
    }

    /// Expands the placeholders. `trees` maps each tree of the bundle to its absolute
    /// directory and `developer_dir` is the selected Xcode's, if one is selected; the
    /// caller guarantees both are normalised absolute paths (as [`crate::materialise`]
    /// does), so the result is normalised.
    pub fn expand(
        &self,
        trees: &BTreeMap<TreeName, String>,
        developer_dir: Option<&str>,
    ) -> Result<String, ExpandError> {
        let mut out = String::with_capacity(self.text.len());
        for segment in &self.segments {
            match segment {
                Segment::Lit(lit) => out.push_str(lit),
                Segment::Tree(name) => out.push_str(
                    trees
                        .get(name)
                        .ok_or_else(|| ExpandError::UnknownTree(name.clone()))?,
                ),
                Segment::DeveloperDir => {
                    out.push_str(developer_dir.ok_or(ExpandError::NoDeveloperDir)?)
                }
            }
        }
        Ok(out)
    }
}

impl TryFrom<String> for Template {
    type Error = TemplateError;

    fn try_from(text: String) -> Result<Template, TemplateError> {
        Template::parse(&text)
    }
}

impl From<Template> for String {
    fn from(template: Template) -> String {
        template.text
    }
}
