//! Emit diagnostics using the `annotate-snippets` library
//!
//! This is the equivalent of `./emitter.rs` but making use of the
//! [`annotate-snippets`][annotate_snippets] library instead of building the output ourselves.
//!
//! [annotate_snippets]: https://docs.rs/crate/annotate-snippets/

use std::borrow::Cow;
use std::error::Report;
use std::fmt::Debug;
use std::io;
use std::sync::Arc;

use annotate_snippets::{AnnotationKind, Group, Padding, Patch, Renderer, Snippet};
use derive_setters::Setters;
use rustc_data_structures::sync::IntoDynSyncSend;
use rustc_error_messages::{FluentArgs, SpanLabel};
use rustc_lint_defs::pluralize;
use rustc_span::source_map::SourceMap;
use rustc_span::{Pos, SourceFile, Span};
use tracing::{debug, info};

use crate::emitter::{
    Destination, MAX_SUGGESTIONS, OutputTheme, is_case_difference, is_different,
    normalize_whitespace, should_show_source_code,
};
use crate::registry::Registry;
use crate::translation::{Translator, to_fluent_args};
use crate::{
    CodeSuggestion, DiagInner, DiagMessage, Emitter, ErrCode, Level, MultiSpan, Style, Subdiag,
    SuggestionStyle, TerminalUrl,
};

/// Default column width, used in tests and when terminal dimensions cannot be determined.
const DEFAULT_COLUMN_WIDTH: usize = 140;

/// Generates diagnostics using annotate-snippet
#[derive(Setters)]
pub struct AnnotateSnippetEmitter {
    /// If true, hides the longer explanation text
    #[setters(skip)]
    dst: IntoDynSyncSend<Destination>,
    sm: Option<Arc<SourceMap>>,
    #[setters(skip)]
    translator: Translator,
    short_message: bool,
    ui_testing: bool,
    ignored_directories_in_source_blocks: Vec<String>,
    diagnostic_width: Option<usize>,

    macro_backtrace: bool,
    track_diagnostics: bool,
    terminal_url: TerminalUrl,
    theme: OutputTheme,
}

impl Debug for AnnotateSnippetEmitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnnotateSnippetEmitter")
            .field("short_message", &self.short_message)
            .field("ui_testing", &self.ui_testing)
            .field(
                "ignored_directories_in_source_blocks",
                &self.ignored_directories_in_source_blocks,
            )
            .field("diagnostic_width", &self.diagnostic_width)
            .field("macro_backtrace", &self.macro_backtrace)
            .field("track_diagnostics", &self.track_diagnostics)
            .field("terminal_url", &self.terminal_url)
            .field("theme", &self.theme)
            .finish()
    }
}

impl Emitter for AnnotateSnippetEmitter {
    /// The entry point for the diagnostics generation
    fn emit_diagnostic(&mut self, mut diag: DiagInner, _registry: &Registry) {
        let fluent_args = to_fluent_args(diag.args.iter());

        if self.track_diagnostics && diag.span.has_primary_spans() && !diag.span.is_dummy() {
            diag.children.insert(0, diag.emitted_at_sub_diag());
        }

        let mut suggestions = diag.suggestions.unwrap_tag();
        self.primary_span_formatted(&mut diag.span, &mut suggestions, &fluent_args);

        self.fix_multispans_in_extern_macros_and_render_macro_backtrace(
            &mut diag.span,
            &mut diag.children,
            &diag.level,
            self.macro_backtrace,
        );

        self.emit_messages_default(
            &diag.level,
            &diag.messages,
            &fluent_args,
            &diag.code,
            &diag.span,
            &diag.children,
            &suggestions,
        );
    }

    fn source_map(&self) -> Option<&SourceMap> {
        self.sm.as_deref()
    }

    fn should_show_explain(&self) -> bool {
        !self.short_message
    }

    fn translator(&self) -> &Translator {
        &self.translator
    }

    fn supports_color(&self) -> bool {
        self.dst.supports_color()
    }
}

fn annotation_level_for_level(level: Level) -> annotate_snippets::level::Level<'static> {
    match level {
        Level::Bug | Level::DelayedBug => {
            annotate_snippets::Level::ERROR.with_name("error: internal compiler error")
        }
        Level::Fatal | Level::Error => annotate_snippets::level::ERROR,
        Level::ForceWarning | Level::Warning => annotate_snippets::Level::WARNING,
        Level::Note | Level::OnceNote => annotate_snippets::Level::NOTE,
        Level::Help | Level::OnceHelp => annotate_snippets::Level::HELP,
        // FIXME(#59346): Not sure how to map this level
        Level::FailureNote => annotate_snippets::Level::NOTE.no_name(),
        Level::Allow => panic!("Should not call with Allow"),
        Level::Expect => panic!("Should not call with Expect"),
    }
}

impl AnnotateSnippetEmitter {
    pub fn new(dst: Destination, translator: Translator) -> Self {
        Self {
            dst: IntoDynSyncSend(dst),
            sm: None,
            translator,
            short_message: false,
            ui_testing: false,
            ignored_directories_in_source_blocks: Vec::new(),
            diagnostic_width: None,
            macro_backtrace: false,
            track_diagnostics: false,
            terminal_url: TerminalUrl::No,
            theme: OutputTheme::Ascii,
        }
    }

    fn emit_messages_default(
        &mut self,
        level: &Level,
        msgs: &[(DiagMessage, Style)],
        args: &FluentArgs<'_>,
        code: &Option<ErrCode>,
        msp: &MultiSpan,
        children: &[Subdiag],
        suggestions: &[CodeSuggestion],
    ) {
        let width = if let Some(width) = self.diagnostic_width {
            width
        } else if self.ui_testing {
            DEFAULT_COLUMN_WIDTH
        } else {
            termize::dimensions().map(|(w, _)| w).unwrap_or(DEFAULT_COLUMN_WIDTH)
        };
        let theme = match self.theme {
            OutputTheme::Ascii => annotate_snippets::renderer::OutputTheme::Ascii,
            OutputTheme::Unicode => annotate_snippets::renderer::OutputTheme::Unicode,
        };

        let anonymized_line_numbers = self.ui_testing;
        let renderer =
            if self.dst.supports_color() { Renderer::styled() } else { Renderer::plain() }
                .term_width(width)
                .anonymized_line_numbers(anonymized_line_numbers)
                .theme(theme)
                .short_message(self.short_message);

        let as_level = annotation_level_for_level(*level);

        // If the destination supports color and at least one portion
        // of the message is styled, we need to "pre-style" the message
        let (message, is_pre_styled) = if self.dst.supports_color()
            && msgs.iter().any(|(_, style)| style != &crate::Style::NoStyle)
        {
            (
                Cow::Owned(
                    msgs.iter()
                        .filter_map(|(m, style)| {
                            let text: String = self
                                .translator
                                .translate_message(m, args)
                                .map_err(Report::new)
                                .unwrap()
                                .to_string();
                            let style = style.anstyle(*level);
                            if text.is_empty() {
                                None
                            } else {
                                Some(format!("{style}{text}{style:#}"))
                            }
                        })
                        .collect::<String>(),
                ),
                true,
            )
        } else {
            (self.translator.translate_messages(msgs, args), false)
        };

        let code = code.map(|c| {
            if let TerminalUrl::Yes = self.terminal_url {
                let path = "https://doc.rust-lang.org/error_codes";
                (c.to_string(), Some(format!("{path}/{c}.html")))
            } else {
                (c.to_string(), None)
            }
        });
        let mut message = if is_pre_styled {
            as_level.pre_styled_title(message)
        } else {
            as_level.title(message)
        };

        if let Some((code, url)) = &code {
            message = message.id(code);
            if let Some(url) = url {
                message = message.id_url(url);
            }
        }
        let mut groups = vec![];
        let mut group = Group::with_title(message);

        let mut file_ann = collect_annotations(self, args, msp);
        // Make sure our primary file comes first
        let primary_span = msp.primary_span().unwrap_or_default();
        let Some(sm) = self.sm.as_ref() else {
            let children = children
                .iter()
                .map(|c| {
                    let msg = self.translator.translate_messages(&c.messages, args).to_string();
                    let level = annotation_level_for_level(c.level);
                    (level, msg)
                })
                .collect::<Vec<_>>();
            if !children.is_empty() {
                for (level, msg) in &children {
                    group = group.element(level.clone().title(msg));
                }
            }
            groups.push(group);
            if let Err(e) = emit_to_destination(
                renderer.render(&groups),
                level,
                &mut self.dst,
                self.short_message,
            ) {
                panic!("failed to emit error: {e}");
            }
            return;
        };

        if !primary_span.is_dummy() {
            let primary_lo = sm.lookup_char_pos(primary_span.lo());
            if let Ok(pos) = file_ann.binary_search_by(|x| x.file.name.cmp(&primary_lo.file.name)) {
                file_ann.swap(0, pos);
            }

            for file in &file_ann {
                let filename = Cow::Owned(
                    sm.filename_for_diagnostics(&file.file.name).to_string_lossy().to_string(),
                );

                let bounding_span =
                    Span::with_root_ctxt(file.file.start_pos, file.file.end_position());
                let source = sm.span_to_snippet(bounding_span).unwrap_or_default();

                if should_show_source_code(
                    &self.ignored_directories_in_source_blocks,
                    sm,
                    &file.file,
                ) {
                    let offset_line = sm.doctest_offset_line(&file.file.name, 1);
                    let snippet = Snippet::source(Cow::Owned(source))
                        .fold(true)
                        .line_start(offset_line)
                        .path(filename)
                        .annotations(file.annotations.iter().map(|h| {
                            let lo = sm.lookup_byte_offset(h.span.lo());
                            let hi = sm.lookup_byte_offset(h.span.hi());
                            let range = lo.pos.to_usize()..hi.pos.to_usize();
                            let ann = h.kind.span(range);
                            if let Some(label) = &h.label { ann.label(label) } else { ann }
                        }));
                    group = group.element(snippet);
                } else if !self.short_message {
                    for (i, h) in file.annotations.iter().enumerate() {
                        if i == 0 || h.label.is_some() {
                            let lo = sm.lookup_char_pos(h.span.lo());

                            let origin = annotate_snippets::Origin::path(filename.clone())
                                .line(sm.doctest_offset_line(&file.file.name, lo.line))
                                .char_column(lo.col_display)
                                .primary(i == 0);
                            group = group.element(origin);
                            if let Some(label) = h.label.as_ref() {
                                if !label.is_empty() {
                                    group = group.element(Padding);
                                    group =
                                        group.element(annotate_snippets::Level::NOTE.title(label));
                                    if i == file.annotations.len() - 1
                                        && (!children.is_empty() || !suggestions.is_empty())
                                    {
                                        group = group.element(Padding);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        for c in children {
            info!("{c:?}");
            let level = annotation_level_for_level(c.level);

            // If the destination supports color and at least one portion
            // of the message is styled, we need to "pre-style" the message
            let (msg, is_pre_styled) = if self.dst.supports_color()
                && c.messages.iter().any(|(_, style)| style != &crate::Style::NoStyle)
            {
                (
                    Cow::Owned(
                        c.messages
                            .iter()
                            .filter_map(|(m, style)| {
                                let text: String = self
                                    .translator
                                    .translate_message(m, args)
                                    .map_err(Report::new)
                                    .unwrap()
                                    .to_string();
                                let style = style.anstyle(c.level);
                                if text.is_empty() {
                                    None
                                } else {
                                    Some(format!("{style}{text}{style:#}"))
                                }
                            })
                            .collect::<String>(),
                    ),
                    true,
                )
            } else {
                (self.translator.translate_messages(&c.messages, args), false)
            };

            if !c.span.has_primary_spans() && !c.span.has_span_labels() {
                group = group.element(level.clone().message(msg));
                continue;
            }

            let title = if is_pre_styled {
                level.clone().pre_styled_title(msg)
            } else {
                level.clone().title(msg)
            };
            let mut temp_group = Group::with_title(title);
            std::mem::swap(&mut group, &mut temp_group);
            groups.push(temp_group);

            let mut file_ann = collect_annotations(self, args, &c.span);
            let primary_span = c.span.primary_span().unwrap_or_default();
            if !primary_span.is_dummy() {
                let primary_lo = sm.lookup_char_pos(primary_span.lo());
                if let Ok(pos) =
                    file_ann.binary_search_by(|x| x.file.name.cmp(&primary_lo.file.name))
                {
                    file_ann.swap(0, pos);
                }
            }

            for file in file_ann {
                let filename = Cow::Owned(
                    sm.filename_for_diagnostics(&file.file.name).to_string_lossy().to_string(),
                );
                let bounding_span =
                    Span::with_root_ctxt(file.file.start_pos, file.file.end_position());
                let source = sm.span_to_snippet(bounding_span).unwrap_or_default();

                if should_show_source_code(
                    &self.ignored_directories_in_source_blocks,
                    sm,
                    &file.file,
                ) {
                    let offset_line = sm.doctest_offset_line(&file.file.name, 1);

                    group = group.element(
                        Snippet::source(Cow::Owned(source))
                            .fold(true)
                            .line_start(offset_line)
                            .path(filename)
                            .annotations(file.annotations.into_iter().map(|h| {
                                let lo = sm.lookup_byte_offset(h.span.lo());
                                let hi = sm.lookup_byte_offset(h.span.hi());

                                let range = lo.pos.to_usize()..hi.pos.to_usize();
                                let ann = h.kind.span(range);
                                if let Some(label) = h.label { ann.label(label) } else { ann }
                            })),
                    );
                } else if !self.short_message {
                    let mut line_tracker = vec![];
                    for (i, h) in file.annotations.into_iter().enumerate() {
                        let lo = sm.lookup_char_pos(h.span.lo());
                        let hi = sm.lookup_char_pos(h.span.hi());
                        if i == 0
                            || (h.label.is_some()
                                && (!line_tracker.contains(&lo.line)
                                    || !line_tracker.contains(&hi.line)))
                        {
                            if !line_tracker.contains(&lo.line) {
                                line_tracker.push(lo.line);
                                let origin = annotate_snippets::Origin::path(filename.clone())
                                    .line(sm.doctest_offset_line(&file.file.name, lo.line))
                                    .char_column(lo.col_display)
                                    .primary(i == 0);
                                group = group.element(origin);
                                if let Some(label) = h.label.clone() {
                                    if !label.is_empty() && lo.line == hi.line {
                                        group = group.element(Padding);
                                        group = group
                                            .element(annotate_snippets::Level::NOTE.title(label));
                                    }
                                }
                            }

                            if let Some(label) = h.label {
                                if !label.is_empty() {
                                    if !line_tracker.contains(&hi.line) {
                                        line_tracker.push(hi.line);
                                        let origin =
                                            annotate_snippets::Origin::path(filename.clone())
                                                .line(
                                                    sm.doctest_offset_line(
                                                        &file.file.name,
                                                        hi.line,
                                                    ),
                                                )
                                                .char_column(hi.col_display)
                                                .primary(false);

                                        group = group.element(origin);
                                        group = group.element(Padding);
                                        group = group
                                            .element(annotate_snippets::Level::NOTE.title(label));
                                    } else if lo.line != hi.line {
                                        group = group.element(Padding);
                                        group = group
                                            .element(annotate_snippets::Level::NOTE.title(label));
                                    }
                                }
                            }
                        } else if let Some(label) = h.label {
                            if !label.is_empty() {
                                group = group.element(Padding);
                                group = group.element(annotate_snippets::Level::NOTE.title(label));
                            }
                        }
                    }
                }
            }
        }

        for suggestion in suggestions {
            match suggestion.style {
                SuggestionStyle::CompletelyHidden => {
                    // do not display this suggestion, it is meant only for tools
                }
                SuggestionStyle::HideCodeAlways => {
                    let msg = self
                        .translator
                        .translate_messages(&[(suggestion.msg.to_owned(), Style::HeaderMsg)], args);
                    group = group.element(annotate_snippets::Level::HELP.title(msg));
                }
                SuggestionStyle::HideCodeInline
                | SuggestionStyle::ShowCode
                | SuggestionStyle::ShowAlways => {
                    let substitutions = suggestion
                        .substitutions
                        .iter()
                        .filter(|subst| {
                            // Suggestions coming from macros can have malformed spans. This is a heavy
                            // handed approach to avoid ICEs by ignoring the suggestion outright.
                            let invalid =
                                subst.parts.iter().any(|item| sm.is_valid_span(item.span).is_err());
                            if invalid {
                                debug!("suggestion contains an invalid span: {:?}", subst);
                            }

                            let Some(item_span) = subst.parts.first() else {
                                return false;
                            };
                            let file = sm.lookup_source_file(item_span.span.lo());
                            !invalid
                                && should_show_source_code(
                                    &self.ignored_directories_in_source_blocks,
                                    sm,
                                    &file,
                                )
                        })
                        .cloned()
                        .collect::<Vec<_>>();
                    if substitutions.is_empty() {
                        continue;
                    }
                    let mut msg = self
                        .translator
                        .translate_message(&suggestion.msg, args)
                        .map_err(Report::new)
                        .unwrap()
                        .to_string();
                    let lo = substitutions
                        .iter()
                        .find_map(|sub| sub.parts.first().map(|p| p.span.lo()))
                        .unwrap();
                    let file = sm.lookup_source_file(lo);
                    if !sm.ensure_source_file_source_present(&file) {
                        continue;
                    }

                    let filename =
                        sm.filename_for_diagnostics(&file.name).to_string_lossy().to_string();

                    let other_suggestions = substitutions.len().saturating_sub(MAX_SUGGESTIONS);
                    let subs = substitutions
                        .clone()
                        .into_iter()
                        .take(MAX_SUGGESTIONS)
                        .filter_map(|sub| {
                            if sub.parts.iter().any(|p| is_case_difference(sm, &p.snippet, p.span))
                            {
                                msg.push_str(" (notice the capitalization difference)");
                            }

                            let parts = sub
                                .parts
                                .into_iter()
                                .filter_map(|p| {
                                    if is_different(sm, &p.snippet, p.span) {
                                        Some((p.span, p.snippet))
                                    } else {
                                        None
                                    }
                                })
                                .collect::<Vec<_>>();
                            if parts.is_empty() {
                                None
                            } else {
                                let lo = parts.iter().map(|(span, _)| span.lo()).min()?;
                                let lo_line = file.lookup_line(file.relative_position(lo))?;
                                let lo = file.line_bounds(lo_line).start;

                                let hi = parts.iter().map(|(span, _)| span.hi()).max()?;
                                let hi_line = file.lookup_line(file.relative_position(hi))?;
                                let hi = file.line_bounds(hi_line).end;

                                Some((Span::with_root_ctxt(lo, hi), lo_line, parts))
                            }
                        })
                        .collect::<Vec<_>>();

                    if !subs.is_empty() {
                        let mut temp_group =
                            Group::with_title(annotate_snippets::Level::HELP.title(msg));
                        std::mem::swap(&mut group, &mut temp_group);
                        groups.push(temp_group);

                        group = group.elements(subs.into_iter().filter_map(
                            |(bounding_span, lo_line, parts)| {
                                // We can't splice anything if the source is unavailable.
                                if let Ok(snippet) = sm.span_to_snippet(bounding_span) {
                                    let adj_lo = bounding_span.lo().to_usize();
                                    Some(
                                        Snippet::source(snippet)
                                            .fold(true)
                                            .line_start(lo_line + 1)
                                            .path(filename.clone())
                                            .patches(parts.into_iter().map(
                                                |(span, replacement)| {
                                                    let lo =
                                                        span.lo().to_usize().saturating_sub(adj_lo);
                                                    let hi =
                                                        span.hi().to_usize().saturating_sub(adj_lo);

                                                    Patch::new(lo..hi, replacement)
                                                },
                                            )),
                                    )
                                } else {
                                    None
                                }
                            },
                        ));
                        if other_suggestions > 0 {
                            group = group.element(
                                annotate_snippets::Level::NOTE.no_name().message(format!(
                                    "and {} other candidate{}",
                                    other_suggestions,
                                    pluralize!(other_suggestions)
                                )),
                            );
                        }
                    }
                }
            }
        }

        // TODO: This hack should be removed once annotate_snippets is the
        // default emitter.
        let suggestions_expected = suggestions
            .iter()
            .filter(|s| {
                matches!(
                    s.style,
                    SuggestionStyle::HideCodeInline
                        | SuggestionStyle::ShowCode
                        | SuggestionStyle::ShowAlways
                )
            })
            .count();
        if suggestions_expected > 0 && groups.is_empty() {
            group = group.element(Padding);
        }

        if !group.is_empty() {
            groups.push(group);
        }
        info!("{groups:#?}");
        if let Err(e) =
            emit_to_destination(renderer.render(&groups), level, &mut self.dst, self.short_message)
        {
            panic!("failed to emit error: {e}");
        }
    }
}

#[derive(Debug)]
struct FileWithAnnotations {
    file: Arc<SourceFile>,
    annotations: Vec<Annotation>,
}

#[derive(Debug)]
struct Annotation {
    kind: AnnotationKind,
    span: Span,
    label: Option<String>,
}

fn collect_annotations(
    emitter: &dyn Emitter,
    args: &FluentArgs<'_>,
    msp: &MultiSpan,
) -> Vec<FileWithAnnotations> {
    fn add_to_file(
        kind: AnnotationKind,
        span: Span,
        label: Option<String>,
        file: Arc<SourceFile>,
        file_vec: &mut Vec<FileWithAnnotations>,
    ) {
        for slot in file_vec.iter_mut() {
            // Look through each of our files for the one we're adding to
            if slot.file.name == file.name {
                slot.annotations.push(Annotation { kind, span, label });
                return;
            }
        }

        file_vec.push(FileWithAnnotations {
            file,
            annotations: vec![Annotation { kind, span, label }],
        });
    }

    let mut output = vec![];

    if let Some(sm) = emitter.source_map() {
        for SpanLabel { span, is_primary, label } in msp.span_labels() {
            // If we don't have a useful span, pick the primary span if that exists.
            // Worst case we'll just print an error at the top of the main file.
            let span = match (span.is_dummy(), msp.primary_span()) {
                (_, None) | (false, _) => span,
                (true, Some(span)) => span,
            };
            let file = sm.lookup_source_file(span.lo());

            let kind = if is_primary { AnnotationKind::Primary } else { AnnotationKind::Context };

            let label = label.as_ref().map(|m| {
                normalize_whitespace(
                    &emitter.translator().translate_message(m, args).map_err(Report::new).unwrap(),
                )
            });

            add_to_file(kind, span, label, file, &mut output);
        }
    }
    output
}

fn emit_to_destination(
    rendered: String,
    lvl: &Level,
    dst: &mut Destination,
    short_message: bool,
) -> io::Result<()> {
    use crate::lock;
    let _buffer_lock = lock::acquire_global_lock("rustc_errors");
    write!(dst, "{rendered}")?;
    if !short_message && !lvl.is_failure_note() {
        writeln!(dst)?;
    }
    dst.flush()?;
    match writeln!(dst) {
        Err(e) => panic!("failed to emit error: {e}"),
        _ => {
            if let Err(e) = dst.flush() {
                panic!("failed to emit error: {e}")
            }
        }
    }
    Ok(())
}
