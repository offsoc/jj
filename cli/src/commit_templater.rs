// Copyright 2020-2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::any::Any;
use std::cmp::max;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::io;
use std::rc::Rc;

use bstr::BString;
use futures::stream::BoxStream;
use futures::StreamExt as _;
use futures::TryStreamExt as _;
use itertools::Itertools as _;
use jj_lib::backend::BackendResult;
use jj_lib::backend::ChangeId;
use jj_lib::backend::CommitId;
use jj_lib::backend::TreeValue;
use jj_lib::commit::Commit;
use jj_lib::conflicts;
use jj_lib::conflicts::ConflictMarkerStyle;
use jj_lib::copies::CopiesTreeDiffEntry;
use jj_lib::copies::CopiesTreeDiffEntryPath;
use jj_lib::copies::CopyRecords;
use jj_lib::extensions_map::ExtensionsMap;
use jj_lib::fileset;
use jj_lib::fileset::FilesetDiagnostics;
use jj_lib::fileset::FilesetExpression;
use jj_lib::id_prefix::IdPrefixContext;
use jj_lib::id_prefix::IdPrefixIndex;
use jj_lib::matchers::Matcher;
use jj_lib::merge::MergedTreeValue;
use jj_lib::merged_tree::MergedTree;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::op_store::RemoteRef;
use jj_lib::ref_name::WorkspaceName;
use jj_lib::ref_name::WorkspaceNameBuf;
use jj_lib::repo::Repo;
use jj_lib::repo_path::RepoPathBuf;
use jj_lib::repo_path::RepoPathUiConverter;
use jj_lib::revset;
use jj_lib::revset::Revset;
use jj_lib::revset::RevsetContainingFn;
use jj_lib::revset::RevsetDiagnostics;
use jj_lib::revset::RevsetModifier;
use jj_lib::revset::RevsetParseContext;
use jj_lib::revset::UserRevsetExpression;
use jj_lib::settings::UserSettings;
use jj_lib::signing::SigStatus;
use jj_lib::signing::SignError;
use jj_lib::signing::SignResult;
use jj_lib::signing::Verification;
use jj_lib::store::Store;
use jj_lib::trailer;
use jj_lib::trailer::Trailer;
use once_cell::unsync::OnceCell;
use pollster::FutureExt as _;

use crate::diff_util;
use crate::diff_util::DiffStats;
use crate::formatter::Formatter;
use crate::revset_util;
use crate::template_builder;
use crate::template_builder::merge_fn_map;
use crate::template_builder::BuildContext;
use crate::template_builder::CoreTemplateBuildFnTable;
use crate::template_builder::CoreTemplatePropertyKind;
use crate::template_builder::IntoTemplateProperty;
use crate::template_builder::TemplateBuildMethodFnMap;
use crate::template_builder::TemplateLanguage;
use crate::template_parser;
use crate::template_parser::ExpressionNode;
use crate::template_parser::FunctionCallNode;
use crate::template_parser::TemplateDiagnostics;
use crate::template_parser::TemplateParseError;
use crate::template_parser::TemplateParseResult;
use crate::templater;
use crate::templater::PlainTextFormattedProperty;
use crate::templater::SizeHint;
use crate::templater::Template;
use crate::templater::TemplateFormatter;
use crate::templater::TemplateProperty;
use crate::templater::TemplatePropertyError;
use crate::templater::TemplatePropertyExt as _;
use crate::text_util;

pub trait CommitTemplateLanguageExtension {
    fn build_fn_table<'repo>(&self) -> CommitTemplateBuildFnTable<'repo>;

    fn build_cache_extensions(&self, extensions: &mut ExtensionsMap);
}

pub struct CommitTemplateLanguage<'repo> {
    repo: &'repo dyn Repo,
    path_converter: &'repo RepoPathUiConverter,
    workspace_name: WorkspaceNameBuf,
    // RevsetParseContext doesn't borrow a repo, but we'll need 'repo lifetime
    // anyway to capture it to evaluate dynamically-constructed user expression
    // such as `revset("ancestors(" ++ commit_id ++ ")")`.
    // TODO: Maybe refactor context structs? RepoPathUiConverter and
    // WorkspaceName are contained in RevsetParseContext for example.
    revset_parse_context: RevsetParseContext<'repo>,
    id_prefix_context: &'repo IdPrefixContext,
    immutable_expression: Rc<UserRevsetExpression>,
    conflict_marker_style: ConflictMarkerStyle,
    build_fn_table: CommitTemplateBuildFnTable<'repo>,
    keyword_cache: CommitKeywordCache<'repo>,
    cache_extensions: ExtensionsMap,
}

impl<'repo> CommitTemplateLanguage<'repo> {
    /// Sets up environment where commit template will be transformed to
    /// evaluation tree.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        repo: &'repo dyn Repo,
        path_converter: &'repo RepoPathUiConverter,
        workspace_name: &WorkspaceName,
        revset_parse_context: RevsetParseContext<'repo>,
        id_prefix_context: &'repo IdPrefixContext,
        immutable_expression: Rc<UserRevsetExpression>,
        conflict_marker_style: ConflictMarkerStyle,
        extensions: &[impl AsRef<dyn CommitTemplateLanguageExtension>],
    ) -> Self {
        let mut build_fn_table = CommitTemplateBuildFnTable::builtin();
        let mut cache_extensions = ExtensionsMap::empty();

        for extension in extensions {
            build_fn_table.merge(extension.as_ref().build_fn_table());
            extension
                .as_ref()
                .build_cache_extensions(&mut cache_extensions);
        }

        CommitTemplateLanguage {
            repo,
            path_converter,
            workspace_name: workspace_name.to_owned(),
            revset_parse_context,
            id_prefix_context,
            immutable_expression,
            conflict_marker_style,
            build_fn_table,
            keyword_cache: CommitKeywordCache::default(),
            cache_extensions,
        }
    }
}

impl<'repo> TemplateLanguage<'repo> for CommitTemplateLanguage<'repo> {
    type Property = CommitTemplatePropertyKind<'repo>;

    template_builder::impl_core_wrap_property_fns!('repo, CommitTemplatePropertyKind::Core);

    fn settings(&self) -> &UserSettings {
        self.repo.base_repo().settings()
    }

    fn build_function(
        &self,
        diagnostics: &mut TemplateDiagnostics,
        build_ctx: &BuildContext<Self::Property>,
        function: &FunctionCallNode,
    ) -> TemplateParseResult<Self::Property> {
        let table = &self.build_fn_table.core;
        table.build_function(self, diagnostics, build_ctx, function)
    }

    fn build_method(
        &self,
        diagnostics: &mut TemplateDiagnostics,
        build_ctx: &BuildContext<Self::Property>,
        property: Self::Property,
        function: &FunctionCallNode,
    ) -> TemplateParseResult<Self::Property> {
        let type_name = property.type_name();
        match property {
            CommitTemplatePropertyKind::Core(property) => {
                let table = &self.build_fn_table.core;
                table.build_method(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::Commit(property) => {
                let table = &self.build_fn_table.commit_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::CommitOpt(property) => {
                let type_name = "Commit";
                let table = &self.build_fn_table.commit_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                let inner_property = property.try_unwrap(type_name);
                build(
                    self,
                    diagnostics,
                    build_ctx,
                    Box::new(inner_property),
                    function,
                )
            }
            CommitTemplatePropertyKind::CommitList(property) => {
                // TODO: migrate to table?
                template_builder::build_unformattable_list_method(
                    self,
                    diagnostics,
                    build_ctx,
                    property,
                    function,
                    Self::wrap_commit,
                    Self::wrap_commit_list,
                )
            }
            CommitTemplatePropertyKind::CommitRef(property) => {
                let table = &self.build_fn_table.commit_ref_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::CommitRefOpt(property) => {
                let type_name = "CommitRef";
                let table = &self.build_fn_table.commit_ref_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                let inner_property = property.try_unwrap(type_name);
                build(
                    self,
                    diagnostics,
                    build_ctx,
                    Box::new(inner_property),
                    function,
                )
            }
            CommitTemplatePropertyKind::CommitRefList(property) => {
                // TODO: migrate to table?
                template_builder::build_formattable_list_method(
                    self,
                    diagnostics,
                    build_ctx,
                    property,
                    function,
                    Self::wrap_commit_ref,
                    Self::wrap_commit_ref_list,
                )
            }
            CommitTemplatePropertyKind::RepoPath(property) => {
                let table = &self.build_fn_table.repo_path_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::RepoPathOpt(property) => {
                let type_name = "RepoPath";
                let table = &self.build_fn_table.repo_path_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                let inner_property = property.try_unwrap(type_name);
                build(
                    self,
                    diagnostics,
                    build_ctx,
                    Box::new(inner_property),
                    function,
                )
            }
            CommitTemplatePropertyKind::CommitOrChangeId(property) => {
                let table = &self.build_fn_table.commit_or_change_id_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::ShortestIdPrefix(property) => {
                let table = &self.build_fn_table.shortest_id_prefix_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::TreeDiff(property) => {
                let table = &self.build_fn_table.tree_diff_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::TreeDiffEntry(property) => {
                let table = &self.build_fn_table.tree_diff_entry_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::TreeDiffEntryList(property) => {
                // TODO: migrate to table?
                template_builder::build_unformattable_list_method(
                    self,
                    diagnostics,
                    build_ctx,
                    property,
                    function,
                    Self::wrap_tree_diff_entry,
                    Self::wrap_tree_diff_entry_list,
                )
            }
            CommitTemplatePropertyKind::TreeEntry(property) => {
                let table = &self.build_fn_table.tree_entry_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::DiffStats(property) => {
                let table = &self.build_fn_table.diff_stats_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                // Strip off formatting parameters which are needed only for the
                // default template output.
                let property = Box::new(property.map(|formatted| formatted.stats));
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::CryptographicSignatureOpt(property) => {
                let type_name = "CryptographicSignature";
                let table = &self.build_fn_table.cryptographic_signature_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                let inner_property = property.try_unwrap(type_name);
                build(
                    self,
                    diagnostics,
                    build_ctx,
                    Box::new(inner_property),
                    function,
                )
            }
            CommitTemplatePropertyKind::AnnotationLine(property) => {
                let type_name = "AnnotationLine";
                let table = &self.build_fn_table.annotation_line_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::Trailer(property) => {
                let table = &self.build_fn_table.trailer_methods;
                let build = template_parser::lookup_method(type_name, table, function)?;
                build(self, diagnostics, build_ctx, property, function)
            }
            CommitTemplatePropertyKind::TrailerList(property) => {
                // TODO: migrate to table?
                template_builder::build_formattable_list_method(
                    self,
                    diagnostics,
                    build_ctx,
                    property,
                    function,
                    Self::wrap_trailer,
                    Self::wrap_trailer_list,
                )
            }
        }
    }
}

// If we need to add multiple languages that support Commit types, this can be
// turned into a trait which extends TemplateLanguage.
impl<'repo> CommitTemplateLanguage<'repo> {
    pub fn repo(&self) -> &'repo dyn Repo {
        self.repo
    }

    pub fn workspace_name(&self) -> &WorkspaceName {
        &self.workspace_name
    }

    pub fn keyword_cache(&self) -> &CommitKeywordCache<'repo> {
        &self.keyword_cache
    }

    pub fn cache_extension<T: Any>(&self) -> Option<&T> {
        self.cache_extensions.get::<T>()
    }

    pub fn wrap_commit(
        property: impl TemplateProperty<Output = Commit> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::Commit(Box::new(property))
    }

    pub fn wrap_commit_opt(
        property: impl TemplateProperty<Output = Option<Commit>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitOpt(Box::new(property))
    }

    pub fn wrap_commit_list(
        property: impl TemplateProperty<Output = Vec<Commit>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitList(Box::new(property))
    }

    pub fn wrap_commit_ref(
        property: impl TemplateProperty<Output = Rc<CommitRef>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitRef(Box::new(property))
    }

    pub fn wrap_commit_ref_opt(
        property: impl TemplateProperty<Output = Option<Rc<CommitRef>>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitRefOpt(Box::new(property))
    }

    pub fn wrap_commit_ref_list(
        property: impl TemplateProperty<Output = Vec<Rc<CommitRef>>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitRefList(Box::new(property))
    }

    pub fn wrap_repo_path(
        property: impl TemplateProperty<Output = RepoPathBuf> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::RepoPath(Box::new(property))
    }

    pub fn wrap_repo_path_opt(
        property: impl TemplateProperty<Output = Option<RepoPathBuf>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::RepoPathOpt(Box::new(property))
    }

    pub fn wrap_commit_or_change_id(
        property: impl TemplateProperty<Output = CommitOrChangeId> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CommitOrChangeId(Box::new(property))
    }

    pub fn wrap_shortest_id_prefix(
        property: impl TemplateProperty<Output = ShortestIdPrefix> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::ShortestIdPrefix(Box::new(property))
    }

    pub fn wrap_tree_diff(
        property: impl TemplateProperty<Output = TreeDiff> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::TreeDiff(Box::new(property))
    }

    pub fn wrap_tree_diff_entry(
        property: impl TemplateProperty<Output = TreeDiffEntry> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::TreeDiffEntry(Box::new(property))
    }

    pub fn wrap_tree_diff_entry_list(
        property: impl TemplateProperty<Output = Vec<TreeDiffEntry>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::TreeDiffEntryList(Box::new(property))
    }

    pub fn wrap_tree_entry(
        property: impl TemplateProperty<Output = TreeEntry> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::TreeEntry(Box::new(property))
    }

    pub fn wrap_diff_stats(
        property: impl TemplateProperty<Output = DiffStatsFormatted<'repo>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::DiffStats(Box::new(property))
    }

    fn wrap_cryptographic_signature_opt(
        property: impl TemplateProperty<Output = Option<CryptographicSignature>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::CryptographicSignatureOpt(Box::new(property))
    }

    pub fn wrap_annotation_line(
        property: impl TemplateProperty<Output = AnnotationLine> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::AnnotationLine(Box::new(property))
    }

    pub fn wrap_trailer(
        property: impl TemplateProperty<Output = Trailer> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::Trailer(Box::new(property))
    }

    pub fn wrap_trailer_list(
        property: impl TemplateProperty<Output = Vec<Trailer>> + 'repo,
    ) -> CommitTemplatePropertyKind<'repo> {
        CommitTemplatePropertyKind::TrailerList(Box::new(property))
    }
}

pub enum CommitTemplatePropertyKind<'repo> {
    Core(CoreTemplatePropertyKind<'repo>),
    Commit(Box<dyn TemplateProperty<Output = Commit> + 'repo>),
    CommitOpt(Box<dyn TemplateProperty<Output = Option<Commit>> + 'repo>),
    CommitList(Box<dyn TemplateProperty<Output = Vec<Commit>> + 'repo>),
    CommitRef(Box<dyn TemplateProperty<Output = Rc<CommitRef>> + 'repo>),
    CommitRefOpt(Box<dyn TemplateProperty<Output = Option<Rc<CommitRef>>> + 'repo>),
    CommitRefList(Box<dyn TemplateProperty<Output = Vec<Rc<CommitRef>>> + 'repo>),
    RepoPath(Box<dyn TemplateProperty<Output = RepoPathBuf> + 'repo>),
    RepoPathOpt(Box<dyn TemplateProperty<Output = Option<RepoPathBuf>> + 'repo>),
    CommitOrChangeId(Box<dyn TemplateProperty<Output = CommitOrChangeId> + 'repo>),
    ShortestIdPrefix(Box<dyn TemplateProperty<Output = ShortestIdPrefix> + 'repo>),
    TreeDiff(Box<dyn TemplateProperty<Output = TreeDiff> + 'repo>),
    TreeDiffEntry(Box<dyn TemplateProperty<Output = TreeDiffEntry> + 'repo>),
    TreeDiffEntryList(Box<dyn TemplateProperty<Output = Vec<TreeDiffEntry>> + 'repo>),
    TreeEntry(Box<dyn TemplateProperty<Output = TreeEntry> + 'repo>),
    DiffStats(Box<dyn TemplateProperty<Output = DiffStatsFormatted<'repo>> + 'repo>),
    CryptographicSignatureOpt(
        Box<dyn TemplateProperty<Output = Option<CryptographicSignature>> + 'repo>,
    ),
    AnnotationLine(Box<dyn TemplateProperty<Output = AnnotationLine> + 'repo>),
    Trailer(Box<dyn TemplateProperty<Output = Trailer> + 'repo>),
    TrailerList(Box<dyn TemplateProperty<Output = Vec<Trailer>> + 'repo>),
}

impl<'repo> IntoTemplateProperty<'repo> for CommitTemplatePropertyKind<'repo> {
    fn type_name(&self) -> &'static str {
        match self {
            CommitTemplatePropertyKind::Core(property) => property.type_name(),
            CommitTemplatePropertyKind::Commit(_) => "Commit",
            CommitTemplatePropertyKind::CommitOpt(_) => "Option<Commit>",
            CommitTemplatePropertyKind::CommitList(_) => "List<Commit>",
            CommitTemplatePropertyKind::CommitRef(_) => "CommitRef",
            CommitTemplatePropertyKind::CommitRefOpt(_) => "Option<CommitRef>",
            CommitTemplatePropertyKind::CommitRefList(_) => "List<CommitRef>",
            CommitTemplatePropertyKind::RepoPath(_) => "RepoPath",
            CommitTemplatePropertyKind::RepoPathOpt(_) => "Option<RepoPath>",
            CommitTemplatePropertyKind::CommitOrChangeId(_) => "CommitOrChangeId",
            CommitTemplatePropertyKind::ShortestIdPrefix(_) => "ShortestIdPrefix",
            CommitTemplatePropertyKind::TreeDiff(_) => "TreeDiff",
            CommitTemplatePropertyKind::TreeDiffEntry(_) => "TreeDiffEntry",
            CommitTemplatePropertyKind::TreeDiffEntryList(_) => "List<TreeDiffEntry>",
            CommitTemplatePropertyKind::TreeEntry(_) => "TreeEntry",
            CommitTemplatePropertyKind::DiffStats(_) => "DiffStats",
            CommitTemplatePropertyKind::CryptographicSignatureOpt(_) => {
                "Option<CryptographicSignature>"
            }
            CommitTemplatePropertyKind::AnnotationLine(_) => "AnnotationLine",
            CommitTemplatePropertyKind::Trailer(_) => "Trailer",
            CommitTemplatePropertyKind::TrailerList(_) => "List<Trailer>",
        }
    }

    fn try_into_boolean(self) -> Option<Box<dyn TemplateProperty<Output = bool> + 'repo>> {
        match self {
            CommitTemplatePropertyKind::Core(property) => property.try_into_boolean(),
            CommitTemplatePropertyKind::Commit(_) => None,
            CommitTemplatePropertyKind::CommitOpt(property) => {
                Some(Box::new(property.map(|opt| opt.is_some())))
            }
            CommitTemplatePropertyKind::CommitList(property) => {
                Some(Box::new(property.map(|l| !l.is_empty())))
            }
            CommitTemplatePropertyKind::CommitRef(_) => None,
            CommitTemplatePropertyKind::CommitRefOpt(property) => {
                Some(Box::new(property.map(|opt| opt.is_some())))
            }
            CommitTemplatePropertyKind::CommitRefList(property) => {
                Some(Box::new(property.map(|l| !l.is_empty())))
            }
            CommitTemplatePropertyKind::RepoPath(_) => None,
            CommitTemplatePropertyKind::RepoPathOpt(property) => {
                Some(Box::new(property.map(|opt| opt.is_some())))
            }
            CommitTemplatePropertyKind::CommitOrChangeId(_) => None,
            CommitTemplatePropertyKind::ShortestIdPrefix(_) => None,
            // TODO: boolean cast could be implemented, but explicit
            // diff.empty() method might be better.
            CommitTemplatePropertyKind::TreeDiff(_) => None,
            CommitTemplatePropertyKind::TreeDiffEntry(_) => None,
            CommitTemplatePropertyKind::TreeDiffEntryList(property) => {
                Some(Box::new(property.map(|l| !l.is_empty())))
            }
            CommitTemplatePropertyKind::TreeEntry(_) => None,
            CommitTemplatePropertyKind::DiffStats(_) => None,
            CommitTemplatePropertyKind::CryptographicSignatureOpt(property) => {
                Some(Box::new(property.map(|sig| sig.is_some())))
            }
            CommitTemplatePropertyKind::AnnotationLine(_) => None,
            CommitTemplatePropertyKind::Trailer(_) => None,
            CommitTemplatePropertyKind::TrailerList(property) => {
                Some(Box::new(property.map(|l| !l.is_empty())))
            }
        }
    }

    fn try_into_integer(self) -> Option<Box<dyn TemplateProperty<Output = i64> + 'repo>> {
        match self {
            CommitTemplatePropertyKind::Core(property) => property.try_into_integer(),
            _ => None,
        }
    }

    fn try_into_plain_text(self) -> Option<Box<dyn TemplateProperty<Output = String> + 'repo>> {
        match self {
            CommitTemplatePropertyKind::Core(property) => property.try_into_plain_text(),
            _ => {
                let template = self.try_into_template()?;
                Some(Box::new(PlainTextFormattedProperty::new(template)))
            }
        }
    }

    fn try_into_template(self) -> Option<Box<dyn Template + 'repo>> {
        match self {
            CommitTemplatePropertyKind::Core(property) => property.try_into_template(),
            CommitTemplatePropertyKind::Commit(_) => None,
            CommitTemplatePropertyKind::CommitOpt(_) => None,
            CommitTemplatePropertyKind::CommitList(_) => None,
            CommitTemplatePropertyKind::CommitRef(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::CommitRefOpt(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::CommitRefList(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::RepoPath(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::RepoPathOpt(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::CommitOrChangeId(property) => {
                Some(property.into_template())
            }
            CommitTemplatePropertyKind::ShortestIdPrefix(property) => {
                Some(property.into_template())
            }
            CommitTemplatePropertyKind::TreeDiff(_) => None,
            CommitTemplatePropertyKind::TreeDiffEntry(_) => None,
            CommitTemplatePropertyKind::TreeDiffEntryList(_) => None,
            CommitTemplatePropertyKind::TreeEntry(_) => None,
            CommitTemplatePropertyKind::DiffStats(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::CryptographicSignatureOpt(_) => None,
            CommitTemplatePropertyKind::AnnotationLine(_) => None,
            CommitTemplatePropertyKind::Trailer(property) => Some(property.into_template()),
            CommitTemplatePropertyKind::TrailerList(property) => Some(property.into_template()),
        }
    }

    fn try_into_eq(self, other: Self) -> Option<Box<dyn TemplateProperty<Output = bool> + 'repo>> {
        match (self, other) {
            (CommitTemplatePropertyKind::Core(lhs), CommitTemplatePropertyKind::Core(rhs)) => {
                lhs.try_into_eq(rhs)
            }
            (CommitTemplatePropertyKind::Core(_), _) => None,
            (CommitTemplatePropertyKind::Commit(_), _) => None,
            (CommitTemplatePropertyKind::CommitOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitList(_), _) => None,
            (CommitTemplatePropertyKind::CommitRef(_), _) => None,
            (CommitTemplatePropertyKind::CommitRefOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitRefList(_), _) => None,
            (CommitTemplatePropertyKind::RepoPath(_), _) => None,
            (CommitTemplatePropertyKind::RepoPathOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitOrChangeId(_), _) => None,
            (CommitTemplatePropertyKind::ShortestIdPrefix(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiff(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiffEntry(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiffEntryList(_), _) => None,
            (CommitTemplatePropertyKind::TreeEntry(_), _) => None,
            (CommitTemplatePropertyKind::DiffStats(_), _) => None,
            (CommitTemplatePropertyKind::CryptographicSignatureOpt(_), _) => None,
            (CommitTemplatePropertyKind::AnnotationLine(_), _) => None,
            (CommitTemplatePropertyKind::Trailer(_), _) => None,
            (CommitTemplatePropertyKind::TrailerList(_), _) => None,
        }
    }

    fn try_into_cmp(
        self,
        other: Self,
    ) -> Option<Box<dyn TemplateProperty<Output = Ordering> + 'repo>> {
        match (self, other) {
            (CommitTemplatePropertyKind::Core(lhs), CommitTemplatePropertyKind::Core(rhs)) => {
                lhs.try_into_cmp(rhs)
            }
            (CommitTemplatePropertyKind::Core(_), _) => None,
            (CommitTemplatePropertyKind::Commit(_), _) => None,
            (CommitTemplatePropertyKind::CommitOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitList(_), _) => None,
            (CommitTemplatePropertyKind::CommitRef(_), _) => None,
            (CommitTemplatePropertyKind::CommitRefOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitRefList(_), _) => None,
            (CommitTemplatePropertyKind::RepoPath(_), _) => None,
            (CommitTemplatePropertyKind::RepoPathOpt(_), _) => None,
            (CommitTemplatePropertyKind::CommitOrChangeId(_), _) => None,
            (CommitTemplatePropertyKind::ShortestIdPrefix(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiff(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiffEntry(_), _) => None,
            (CommitTemplatePropertyKind::TreeDiffEntryList(_), _) => None,
            (CommitTemplatePropertyKind::TreeEntry(_), _) => None,
            (CommitTemplatePropertyKind::DiffStats(_), _) => None,
            (CommitTemplatePropertyKind::CryptographicSignatureOpt(_), _) => None,
            (CommitTemplatePropertyKind::AnnotationLine(_), _) => None,
            (CommitTemplatePropertyKind::Trailer(_), _) => None,
            (CommitTemplatePropertyKind::TrailerList(_), _) => None,
        }
    }
}

/// Table of functions that translate method call node of self type `T`.
pub type CommitTemplateBuildMethodFnMap<'repo, T> =
    TemplateBuildMethodFnMap<'repo, CommitTemplateLanguage<'repo>, T>;

/// Symbol table of methods available in the commit template.
pub struct CommitTemplateBuildFnTable<'repo> {
    pub core: CoreTemplateBuildFnTable<'repo, CommitTemplateLanguage<'repo>>,
    pub commit_methods: CommitTemplateBuildMethodFnMap<'repo, Commit>,
    pub commit_ref_methods: CommitTemplateBuildMethodFnMap<'repo, Rc<CommitRef>>,
    pub repo_path_methods: CommitTemplateBuildMethodFnMap<'repo, RepoPathBuf>,
    pub commit_or_change_id_methods: CommitTemplateBuildMethodFnMap<'repo, CommitOrChangeId>,
    pub shortest_id_prefix_methods: CommitTemplateBuildMethodFnMap<'repo, ShortestIdPrefix>,
    pub tree_diff_methods: CommitTemplateBuildMethodFnMap<'repo, TreeDiff>,
    pub tree_diff_entry_methods: CommitTemplateBuildMethodFnMap<'repo, TreeDiffEntry>,
    pub tree_entry_methods: CommitTemplateBuildMethodFnMap<'repo, TreeEntry>,
    pub diff_stats_methods: CommitTemplateBuildMethodFnMap<'repo, DiffStats>,
    pub cryptographic_signature_methods:
        CommitTemplateBuildMethodFnMap<'repo, CryptographicSignature>,
    pub annotation_line_methods: CommitTemplateBuildMethodFnMap<'repo, AnnotationLine>,
    pub trailer_methods: CommitTemplateBuildMethodFnMap<'repo, Trailer>,
}

impl<'repo> CommitTemplateBuildFnTable<'repo> {
    /// Creates new symbol table containing the builtin methods.
    fn builtin() -> Self {
        CommitTemplateBuildFnTable {
            core: CoreTemplateBuildFnTable::builtin(),
            commit_methods: builtin_commit_methods(),
            commit_ref_methods: builtin_commit_ref_methods(),
            repo_path_methods: builtin_repo_path_methods(),
            commit_or_change_id_methods: builtin_commit_or_change_id_methods(),
            shortest_id_prefix_methods: builtin_shortest_id_prefix_methods(),
            tree_diff_methods: builtin_tree_diff_methods(),
            tree_diff_entry_methods: builtin_tree_diff_entry_methods(),
            tree_entry_methods: builtin_tree_entry_methods(),
            diff_stats_methods: builtin_diff_stats_methods(),
            cryptographic_signature_methods: builtin_cryptographic_signature_methods(),
            annotation_line_methods: builtin_annotation_line_methods(),
            trailer_methods: builtin_trailer_methods(),
        }
    }

    pub fn empty() -> Self {
        CommitTemplateBuildFnTable {
            core: CoreTemplateBuildFnTable::empty(),
            commit_methods: HashMap::new(),
            commit_ref_methods: HashMap::new(),
            repo_path_methods: HashMap::new(),
            commit_or_change_id_methods: HashMap::new(),
            shortest_id_prefix_methods: HashMap::new(),
            tree_diff_methods: HashMap::new(),
            tree_diff_entry_methods: HashMap::new(),
            tree_entry_methods: HashMap::new(),
            diff_stats_methods: HashMap::new(),
            cryptographic_signature_methods: HashMap::new(),
            annotation_line_methods: HashMap::new(),
            trailer_methods: HashMap::new(),
        }
    }

    fn merge(&mut self, extension: CommitTemplateBuildFnTable<'repo>) {
        let CommitTemplateBuildFnTable {
            core,
            commit_methods,
            commit_ref_methods,
            repo_path_methods,
            commit_or_change_id_methods,
            shortest_id_prefix_methods,
            tree_diff_methods,
            tree_diff_entry_methods,
            tree_entry_methods,
            diff_stats_methods,
            cryptographic_signature_methods,
            annotation_line_methods,
            trailer_methods,
        } = extension;

        self.core.merge(core);
        merge_fn_map(&mut self.commit_methods, commit_methods);
        merge_fn_map(&mut self.commit_ref_methods, commit_ref_methods);
        merge_fn_map(&mut self.repo_path_methods, repo_path_methods);
        merge_fn_map(
            &mut self.commit_or_change_id_methods,
            commit_or_change_id_methods,
        );
        merge_fn_map(
            &mut self.shortest_id_prefix_methods,
            shortest_id_prefix_methods,
        );
        merge_fn_map(&mut self.tree_diff_methods, tree_diff_methods);
        merge_fn_map(&mut self.tree_diff_entry_methods, tree_diff_entry_methods);
        merge_fn_map(&mut self.tree_entry_methods, tree_entry_methods);
        merge_fn_map(&mut self.diff_stats_methods, diff_stats_methods);
        merge_fn_map(
            &mut self.cryptographic_signature_methods,
            cryptographic_signature_methods,
        );
        merge_fn_map(&mut self.annotation_line_methods, annotation_line_methods);
        merge_fn_map(&mut self.trailer_methods, trailer_methods);
    }
}

#[derive(Default)]
pub struct CommitKeywordCache<'repo> {
    // Build index lazily, and Rc to get away from &self lifetime.
    bookmarks_index: OnceCell<Rc<CommitRefsIndex>>,
    tags_index: OnceCell<Rc<CommitRefsIndex>>,
    git_refs_index: OnceCell<Rc<CommitRefsIndex>>,
    is_immutable_fn: OnceCell<Rc<RevsetContainingFn<'repo>>>,
}

impl<'repo> CommitKeywordCache<'repo> {
    pub fn bookmarks_index(&self, repo: &dyn Repo) -> &Rc<CommitRefsIndex> {
        self.bookmarks_index
            .get_or_init(|| Rc::new(build_bookmarks_index(repo)))
    }

    pub fn tags_index(&self, repo: &dyn Repo) -> &Rc<CommitRefsIndex> {
        self.tags_index
            .get_or_init(|| Rc::new(build_commit_refs_index(repo.view().tags())))
    }

    pub fn git_refs_index(&self, repo: &dyn Repo) -> &Rc<CommitRefsIndex> {
        self.git_refs_index
            .get_or_init(|| Rc::new(build_commit_refs_index(repo.view().git_refs())))
    }

    pub fn is_immutable_fn(
        &self,
        language: &CommitTemplateLanguage<'repo>,
        span: pest::Span<'_>,
    ) -> TemplateParseResult<&Rc<RevsetContainingFn<'repo>>> {
        // Alternatively, a negated (i.e. visible mutable) set could be computed.
        // It's usually smaller than the immutable set. The revset engine can also
        // optimize "::<recent_heads>" query to use bitset-based implementation.
        self.is_immutable_fn.get_or_try_init(|| {
            let expression = &language.immutable_expression;
            let revset = evaluate_revset_expression(language, span, expression)?;
            Ok(revset.containing_fn().into())
        })
    }
}

fn builtin_commit_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, Commit> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<Commit>::new();
    map.insert(
        "description",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|commit| text_util::complete_newline(commit.description()));
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "trailers",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property
                .map(|commit| trailer::parse_description_trailers(commit.description()));
            Ok(L::wrap_trailer_list(out_property))
        },
    );
    map.insert(
        "change_id",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|commit| CommitOrChangeId::Change(commit.change_id().to_owned()));
            Ok(L::wrap_commit_or_change_id(out_property))
        },
    );
    map.insert(
        "commit_id",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|commit| CommitOrChangeId::Commit(commit.id().to_owned()));
            Ok(L::wrap_commit_or_change_id(out_property))
        },
    );
    map.insert(
        "parents",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.and_then(|commit| Ok(commit.parents().try_collect()?));
            Ok(L::wrap_commit_list(out_property))
        },
    );
    map.insert(
        "author",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit| commit.author().clone());
            Ok(L::wrap_signature(out_property))
        },
    );
    map.insert(
        "committer",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit| commit.committer().clone());
            Ok(L::wrap_signature(out_property))
        },
    );
    map.insert(
        "mine",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let user_email = language.revset_parse_context.user_email.to_owned();
            let out_property = self_property.map(move |commit| commit.author().email == user_email);
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "signature",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(CryptographicSignature::new);
            Ok(L::wrap_cryptographic_signature_opt(out_property))
        },
    );
    map.insert(
        "working_copies",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.map(|commit| extract_working_copies(repo, &commit));
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "current_working_copy",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let name = language.workspace_name.clone();
            let out_property = self_property
                .map(move |commit| Some(commit.id()) == repo.view().get_wc_commit_id(&name));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "bookmarks",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let index = language
                .keyword_cache
                .bookmarks_index(language.repo)
                .clone();
            let out_property = self_property.map(move |commit| {
                index
                    .get(commit.id())
                    .iter()
                    .filter(|commit_ref| commit_ref.is_local() || !commit_ref.synced)
                    .cloned()
                    .collect()
            });
            Ok(L::wrap_commit_ref_list(out_property))
        },
    );
    map.insert(
        "local_bookmarks",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let index = language
                .keyword_cache
                .bookmarks_index(language.repo)
                .clone();
            let out_property = self_property.map(move |commit| {
                index
                    .get(commit.id())
                    .iter()
                    .filter(|commit_ref| commit_ref.is_local())
                    .cloned()
                    .collect()
            });
            Ok(L::wrap_commit_ref_list(out_property))
        },
    );
    map.insert(
        "remote_bookmarks",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let index = language
                .keyword_cache
                .bookmarks_index(language.repo)
                .clone();
            let out_property = self_property.map(move |commit| {
                index
                    .get(commit.id())
                    .iter()
                    .filter(|commit_ref| commit_ref.is_remote())
                    .cloned()
                    .collect()
            });
            Ok(L::wrap_commit_ref_list(out_property))
        },
    );
    map.insert(
        "tags",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let index = language.keyword_cache.tags_index(language.repo).clone();
            let out_property = self_property.map(move |commit| index.get(commit.id()).to_vec());
            Ok(L::wrap_commit_ref_list(out_property))
        },
    );
    map.insert(
        "git_refs",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let index = language.keyword_cache.git_refs_index(language.repo).clone();
            let out_property = self_property.map(move |commit| index.get(commit.id()).to_vec());
            Ok(L::wrap_commit_ref_list(out_property))
        },
    );
    map.insert(
        "git_head",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.map(|commit| {
                let target = repo.view().git_head();
                target.added_ids().contains(commit.id())
            });
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "divergent",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.map(|commit| {
                // The given commit could be hidden in e.g. `jj evolog`.
                let maybe_entries = repo.resolve_change_id(commit.change_id());
                maybe_entries.map_or(0, |entries| entries.len()) > 1
            });
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "hidden",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.map(|commit| commit.is_hidden(repo));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "immutable",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let is_immutable = language
                .keyword_cache
                .is_immutable_fn(language, function.name_span)?
                .clone();
            let out_property = self_property.and_then(move |commit| Ok(is_immutable(commit.id())?));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "contained_in",
        |language, diagnostics, _build_ctx, self_property, function| {
            let [revset_node] = function.expect_exact_arguments()?;

            let is_contained =
                template_parser::expect_string_literal_with(revset_node, |revset, span| {
                    Ok(evaluate_user_revset(language, diagnostics, span, revset)?.containing_fn())
                })?;

            let out_property = self_property.and_then(move |commit| Ok(is_contained(commit.id())?));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "conflict",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.and_then(|commit| Ok(commit.has_conflict()?));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "empty",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.and_then(|commit| Ok(commit.is_empty(repo)?));
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "diff",
        |language, diagnostics, _build_ctx, self_property, function| {
            let ([], [files_node]) = function.expect_arguments()?;
            let files = if let Some(node) = files_node {
                expect_fileset_literal(diagnostics, node, language.path_converter)?
            } else {
                // TODO: defaults to CLI path arguments?
                // https://github.com/jj-vcs/jj/issues/2933#issuecomment-1925870731
                FilesetExpression::all()
            };
            let repo = language.repo;
            let matcher: Rc<dyn Matcher> = files.to_matcher().into();
            let out_property = self_property
                .and_then(move |commit| Ok(TreeDiff::from_commit(repo, &commit, matcher.clone())?));
            Ok(L::wrap_tree_diff(out_property))
        },
    );
    map.insert(
        "root",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property =
                self_property.map(|commit| commit.id() == repo.store().root_commit_id());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map
}

// TODO: return Vec<String>
fn extract_working_copies(repo: &dyn Repo, commit: &Commit) -> String {
    let wc_commit_ids = repo.view().wc_commit_ids();
    if wc_commit_ids.len() <= 1 {
        return "".to_string();
    }
    let mut names = vec![];
    for (name, wc_commit_id) in wc_commit_ids {
        if wc_commit_id == commit.id() {
            names.push(format!("{}@", name.as_symbol()));
        }
    }
    names.join(" ")
}

fn expect_fileset_literal(
    diagnostics: &mut TemplateDiagnostics,
    node: &ExpressionNode,
    path_converter: &RepoPathUiConverter,
) -> Result<FilesetExpression, TemplateParseError> {
    template_parser::expect_string_literal_with(node, |text, span| {
        let mut inner_diagnostics = FilesetDiagnostics::new();
        let expression =
            fileset::parse(&mut inner_diagnostics, text, path_converter).map_err(|err| {
                TemplateParseError::expression("In fileset expression", span).with_source(err)
            })?;
        diagnostics.extend_with(inner_diagnostics, |diag| {
            TemplateParseError::expression("In fileset expression", span).with_source(diag)
        });
        Ok(expression)
    })
}

fn evaluate_revset_expression<'repo>(
    language: &CommitTemplateLanguage<'repo>,
    span: pest::Span<'_>,
    expression: &UserRevsetExpression,
) -> Result<Box<dyn Revset + 'repo>, TemplateParseError> {
    let make_error = || TemplateParseError::expression("Failed to evaluate revset", span);
    let repo = language.repo;
    let symbol_resolver = revset_util::default_symbol_resolver(
        repo,
        language.revset_parse_context.extensions.symbol_resolvers(),
        language.id_prefix_context,
    );
    let revset = expression
        .resolve_user_expression(repo, &symbol_resolver)
        .map_err(|err| make_error().with_source(err))?
        .evaluate(repo)
        .map_err(|err| make_error().with_source(err))?;
    Ok(revset)
}

fn evaluate_user_revset<'repo>(
    language: &CommitTemplateLanguage<'repo>,
    diagnostics: &mut TemplateDiagnostics,
    span: pest::Span<'_>,
    revset: &str,
) -> Result<Box<dyn Revset + 'repo>, TemplateParseError> {
    let mut inner_diagnostics = RevsetDiagnostics::new();
    let (expression, modifier) = revset::parse_with_modifier(
        &mut inner_diagnostics,
        revset,
        &language.revset_parse_context,
    )
    .map_err(|err| TemplateParseError::expression("In revset expression", span).with_source(err))?;
    diagnostics.extend_with(inner_diagnostics, |diag| {
        TemplateParseError::expression("In revset expression", span).with_source(diag)
    });
    let (None | Some(RevsetModifier::All)) = modifier;

    evaluate_revset_expression(language, span, &expression)
}

/// Bookmark or tag name with metadata.
#[derive(Debug)]
pub struct CommitRef {
    /// Local name.
    name: String,
    /// Remote name if this is a remote or Git-tracking ref.
    remote: Option<String>,
    /// Target commit ids.
    target: RefTarget,
    /// Local ref metadata which tracks this remote ref.
    tracking_ref: Option<TrackingRef>,
    /// Local ref is synchronized with all tracking remotes, or tracking remote
    /// ref is synchronized with the local.
    synced: bool,
}

#[derive(Debug)]
struct TrackingRef {
    /// Local ref target which tracks the other remote ref.
    target: RefTarget,
    /// Number of commits ahead of the tracking `target`.
    ahead_count: OnceCell<SizeHint>,
    /// Number of commits behind of the tracking `target`.
    behind_count: OnceCell<SizeHint>,
}

impl CommitRef {
    // CommitRef is wrapped by Rc<T> to make it cheaply cloned and share
    // lazy-evaluation results across clones.

    /// Creates local ref representation which might track some of the
    /// `remote_refs`.
    pub fn local<'a>(
        name: impl Into<String>,
        target: RefTarget,
        remote_refs: impl IntoIterator<Item = &'a RemoteRef>,
    ) -> Rc<Self> {
        let synced = remote_refs
            .into_iter()
            .all(|remote_ref| !remote_ref.is_tracked() || remote_ref.target == target);
        Rc::new(CommitRef {
            name: name.into(),
            remote: None,
            target,
            tracking_ref: None,
            synced,
        })
    }

    /// Creates local ref representation which doesn't track any remote refs.
    pub fn local_only(name: impl Into<String>, target: RefTarget) -> Rc<Self> {
        Self::local(name, target, [])
    }

    /// Creates remote ref representation which might be tracked by a local ref
    /// pointing to the `local_target`.
    pub fn remote(
        name: impl Into<String>,
        remote_name: impl Into<String>,
        remote_ref: RemoteRef,
        local_target: &RefTarget,
    ) -> Rc<Self> {
        let synced = remote_ref.is_tracked() && remote_ref.target == *local_target;
        let tracking_ref = remote_ref.is_tracked().then(|| {
            let count = if synced {
                OnceCell::from((0, Some(0))) // fast path for synced remotes
            } else {
                OnceCell::new()
            };
            TrackingRef {
                target: local_target.clone(),
                ahead_count: count.clone(),
                behind_count: count,
            }
        });
        Rc::new(CommitRef {
            name: name.into(),
            remote: Some(remote_name.into()),
            target: remote_ref.target,
            tracking_ref,
            synced,
        })
    }

    /// Creates remote ref representation which isn't tracked by a local ref.
    pub fn remote_only(
        name: impl Into<String>,
        remote_name: impl Into<String>,
        target: RefTarget,
    ) -> Rc<Self> {
        Rc::new(CommitRef {
            name: name.into(),
            remote: Some(remote_name.into()),
            target,
            tracking_ref: None,
            synced: false, // has no local counterpart
        })
    }

    /// Local name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Remote name if this is a remote or Git-tracking ref.
    pub fn remote_name(&self) -> Option<&str> {
        self.remote.as_deref()
    }

    /// Target commit ids.
    pub fn target(&self) -> &RefTarget {
        &self.target
    }

    /// Returns true if this is a local ref.
    pub fn is_local(&self) -> bool {
        self.remote.is_none()
    }

    /// Returns true if this is a remote ref.
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// Returns true if this ref points to no commit.
    pub fn is_absent(&self) -> bool {
        self.target.is_absent()
    }

    /// Returns true if this ref points to any commit.
    pub fn is_present(&self) -> bool {
        self.target.is_present()
    }

    /// Whether the ref target has conflicts.
    pub fn has_conflict(&self) -> bool {
        self.target.has_conflict()
    }

    /// Returns true if this ref is tracked by a local ref. The local ref might
    /// have been deleted (but not pushed yet.)
    pub fn is_tracked(&self) -> bool {
        self.tracking_ref.is_some()
    }

    /// Returns true if this ref is tracked by a local ref, and if the local ref
    /// is present.
    pub fn is_tracking_present(&self) -> bool {
        self.tracking_ref
            .as_ref()
            .is_some_and(|tracking| tracking.target.is_present())
    }

    /// Number of commits ahead of the tracking local ref.
    fn tracking_ahead_count(&self, repo: &dyn Repo) -> Result<SizeHint, TemplatePropertyError> {
        let Some(tracking) = &self.tracking_ref else {
            return Err(TemplatePropertyError("Not a tracked remote ref".into()));
        };
        tracking
            .ahead_count
            .get_or_try_init(|| {
                let self_ids = self.target.added_ids().cloned().collect_vec();
                let other_ids = tracking.target.added_ids().cloned().collect_vec();
                Ok(revset::walk_revs(repo, &self_ids, &other_ids)?.count_estimate()?)
            })
            .copied()
    }

    /// Number of commits behind of the tracking local ref.
    fn tracking_behind_count(&self, repo: &dyn Repo) -> Result<SizeHint, TemplatePropertyError> {
        let Some(tracking) = &self.tracking_ref else {
            return Err(TemplatePropertyError("Not a tracked remote ref".into()));
        };
        tracking
            .behind_count
            .get_or_try_init(|| {
                let self_ids = self.target.added_ids().cloned().collect_vec();
                let other_ids = tracking.target.added_ids().cloned().collect_vec();
                Ok(revset::walk_revs(repo, &other_ids, &self_ids)?.count_estimate()?)
            })
            .copied()
    }
}

// If wrapping with Rc<T> becomes common, add generic impl for Rc<T>.
impl Template for Rc<CommitRef> {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        write!(formatter.labeled("name"), "{}", self.name)?;
        if let Some(remote) = &self.remote {
            write!(formatter, "@")?;
            write!(formatter.labeled("remote"), "{remote}")?;
        }
        // Don't show both conflict and unsynced sigils as conflicted ref wouldn't
        // be pushed.
        if self.has_conflict() {
            write!(formatter, "??")?;
        } else if self.is_local() && !self.synced {
            write!(formatter, "*")?;
        }
        Ok(())
    }
}

impl Template for Vec<Rc<CommitRef>> {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        templater::format_joined(formatter, self, " ")
    }
}

fn builtin_commit_ref_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, Rc<CommitRef>> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<Rc<CommitRef>>::new();
    map.insert(
        "name",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit_ref| commit_ref.name.clone());
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "remote",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|commit_ref| commit_ref.remote.clone().unwrap_or_default());
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "present",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit_ref| commit_ref.is_present());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "conflict",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit_ref| commit_ref.has_conflict());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "normal_target",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.and_then(|commit_ref| {
                let maybe_id = commit_ref.target.as_normal();
                Ok(maybe_id.map(|id| repo.store().get_commit(id)).transpose()?)
            });
            Ok(L::wrap_commit_opt(out_property))
        },
    );
    map.insert(
        "removed_targets",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.and_then(|commit_ref| {
                let ids = commit_ref.target.removed_ids();
                Ok(ids.map(|id| repo.store().get_commit(id)).try_collect()?)
            });
            Ok(L::wrap_commit_list(out_property))
        },
    );
    map.insert(
        "added_targets",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property = self_property.and_then(|commit_ref| {
                let ids = commit_ref.target.added_ids();
                Ok(ids.map(|id| repo.store().get_commit(id)).try_collect()?)
            });
            Ok(L::wrap_commit_list(out_property))
        },
    );
    map.insert(
        "tracked",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit_ref| commit_ref.is_tracked());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "tracking_present",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|commit_ref| commit_ref.is_tracking_present());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "tracking_ahead_count",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property =
                self_property.and_then(|commit_ref| commit_ref.tracking_ahead_count(repo));
            Ok(L::wrap_size_hint(out_property))
        },
    );
    map.insert(
        "tracking_behind_count",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let repo = language.repo;
            let out_property =
                self_property.and_then(|commit_ref| commit_ref.tracking_behind_count(repo));
            Ok(L::wrap_size_hint(out_property))
        },
    );
    map
}

/// Cache for reverse lookup refs.
#[derive(Clone, Debug, Default)]
pub struct CommitRefsIndex {
    index: HashMap<CommitId, Vec<Rc<CommitRef>>>,
}

impl CommitRefsIndex {
    fn insert<'a>(&mut self, ids: impl IntoIterator<Item = &'a CommitId>, name: Rc<CommitRef>) {
        for id in ids {
            let commit_refs = self.index.entry(id.clone()).or_default();
            commit_refs.push(name.clone());
        }
    }

    pub fn get(&self, id: &CommitId) -> &[Rc<CommitRef>] {
        self.index.get(id).map_or(&[], |refs: &Vec<_>| refs)
    }
}

fn build_bookmarks_index(repo: &dyn Repo) -> CommitRefsIndex {
    let mut index = CommitRefsIndex::default();
    for (bookmark_name, bookmark_target) in repo.view().bookmarks() {
        let local_target = bookmark_target.local_target;
        let remote_refs = bookmark_target.remote_refs;
        if local_target.is_present() {
            let commit_ref = CommitRef::local(
                bookmark_name,
                local_target.clone(),
                remote_refs.iter().map(|&(_, remote_ref)| remote_ref),
            );
            index.insert(local_target.added_ids(), commit_ref);
        }
        for &(remote_name, remote_ref) in &remote_refs {
            let commit_ref =
                CommitRef::remote(bookmark_name, remote_name, remote_ref.clone(), local_target);
            index.insert(remote_ref.target.added_ids(), commit_ref);
        }
    }
    index
}

fn build_commit_refs_index<'a, K: Into<String>>(
    ref_pairs: impl IntoIterator<Item = (K, &'a RefTarget)>,
) -> CommitRefsIndex {
    let mut index = CommitRefsIndex::default();
    for (name, target) in ref_pairs {
        let commit_ref = CommitRef::local_only(name, target.clone());
        index.insert(target.added_ids(), commit_ref);
    }
    index
}

impl Template for RepoPathBuf {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        write!(formatter, "{}", self.as_internal_file_string())
    }
}

fn builtin_repo_path_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, RepoPathBuf> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<RepoPathBuf>::new();
    map.insert(
        "display",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let path_converter = language.path_converter;
            let out_property = self_property.map(|path| path_converter.format_file_path(&path));
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "parent",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|path| Some(path.parent()?.to_owned()));
            Ok(L::wrap_repo_path_opt(out_property))
        },
    );
    map
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitOrChangeId {
    Commit(CommitId),
    Change(ChangeId),
}

impl CommitOrChangeId {
    pub fn hex(&self) -> String {
        match self {
            CommitOrChangeId::Commit(id) => id.hex(),
            CommitOrChangeId::Change(id) => id.reverse_hex(),
        }
    }

    pub fn short(&self, total_len: usize) -> String {
        let mut hex = self.hex();
        hex.truncate(total_len);
        hex
    }

    /// The length of the id printed will be the maximum of `total_len` and the
    /// length of the shortest unique prefix
    pub fn shortest(
        &self,
        repo: &dyn Repo,
        index: &IdPrefixIndex,
        total_len: usize,
    ) -> ShortestIdPrefix {
        let mut hex = self.hex();
        let prefix_len = match self {
            CommitOrChangeId::Commit(id) => index.shortest_commit_prefix_len(repo, id),
            CommitOrChangeId::Change(id) => index.shortest_change_prefix_len(repo, id),
        };
        hex.truncate(max(prefix_len, total_len));
        let rest = hex.split_off(prefix_len);
        ShortestIdPrefix { prefix: hex, rest }
    }
}

impl Template for CommitOrChangeId {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        write!(formatter, "{}", self.hex())
    }
}

fn builtin_commit_or_change_id_methods<'repo>(
) -> CommitTemplateBuildMethodFnMap<'repo, CommitOrChangeId> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<CommitOrChangeId>::new();
    map.insert(
        "normal_hex",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            Ok(L::wrap_string(self_property.map(|id| {
                // Note: this is _not_ the same as id.hex() for ChangeId, which
                // returns the "reverse" hex (z-k), instead of the "forward" /
                // normal hex (0-9a-f) we want here.
                match id {
                    CommitOrChangeId::Commit(id) => id.hex(),
                    CommitOrChangeId::Change(id) => id.hex(),
                }
            })))
        },
    );
    map.insert(
        "short",
        |language, diagnostics, build_ctx, self_property, function| {
            let ([], [len_node]) = function.expect_arguments()?;
            let len_property = len_node
                .map(|node| {
                    template_builder::expect_usize_expression(
                        language,
                        diagnostics,
                        build_ctx,
                        node,
                    )
                })
                .transpose()?;
            let out_property =
                (self_property, len_property).map(|(id, len)| id.short(len.unwrap_or(12)));
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "shortest",
        |language, diagnostics, build_ctx, self_property, function| {
            let ([], [len_node]) = function.expect_arguments()?;
            let len_property = len_node
                .map(|node| {
                    template_builder::expect_usize_expression(
                        language,
                        diagnostics,
                        build_ctx,
                        node,
                    )
                })
                .transpose()?;
            let repo = language.repo;
            let index = match language.id_prefix_context.populate(repo) {
                Ok(index) => index,
                Err(err) => {
                    // Not an error because we can still produce somewhat
                    // reasonable output.
                    diagnostics.add_warning(
                        TemplateParseError::expression(
                            "Failed to load short-prefixes index",
                            function.name_span,
                        )
                        .with_source(err),
                    );
                    IdPrefixIndex::empty()
                }
            };
            let out_property = (self_property, len_property)
                .map(move |(id, len)| id.shortest(repo, &index, len.unwrap_or(0)));
            Ok(L::wrap_shortest_id_prefix(out_property))
        },
    );
    map
}

pub struct ShortestIdPrefix {
    pub prefix: String,
    pub rest: String,
}

impl Template for ShortestIdPrefix {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        write!(formatter.labeled("prefix"), "{}", self.prefix)?;
        write!(formatter.labeled("rest"), "{}", self.rest)?;
        Ok(())
    }
}

impl ShortestIdPrefix {
    fn to_upper(&self) -> Self {
        Self {
            prefix: self.prefix.to_ascii_uppercase(),
            rest: self.rest.to_ascii_uppercase(),
        }
    }
    fn to_lower(&self) -> Self {
        Self {
            prefix: self.prefix.to_ascii_lowercase(),
            rest: self.rest.to_ascii_lowercase(),
        }
    }
}

fn builtin_shortest_id_prefix_methods<'repo>(
) -> CommitTemplateBuildMethodFnMap<'repo, ShortestIdPrefix> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<ShortestIdPrefix>::new();
    map.insert(
        "prefix",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|id| id.prefix);
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "rest",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|id| id.rest);
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "upper",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|id| id.to_upper());
            Ok(L::wrap_shortest_id_prefix(out_property))
        },
    );
    map.insert(
        "lower",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|id| id.to_lower());
            Ok(L::wrap_shortest_id_prefix(out_property))
        },
    );
    map
}

/// Pair of trees to be diffed.
#[derive(Debug)]
pub struct TreeDiff {
    from_tree: MergedTree,
    to_tree: MergedTree,
    matcher: Rc<dyn Matcher>,
    copy_records: CopyRecords,
}

impl TreeDiff {
    fn from_commit(
        repo: &dyn Repo,
        commit: &Commit,
        matcher: Rc<dyn Matcher>,
    ) -> BackendResult<Self> {
        let mut copy_records = CopyRecords::default();
        for parent in commit.parent_ids() {
            let records =
                diff_util::get_copy_records(repo.store(), parent, commit.id(), &*matcher)?;
            copy_records.add_records(records)?;
        }
        Ok(TreeDiff {
            from_tree: commit.parent_tree(repo)?,
            to_tree: commit.tree()?,
            matcher,
            copy_records,
        })
    }

    fn diff_stream(&self) -> BoxStream<'_, CopiesTreeDiffEntry> {
        self.from_tree
            .diff_stream_with_copies(&self.to_tree, &*self.matcher, &self.copy_records)
    }

    async fn collect_entries(&self) -> BackendResult<Vec<TreeDiffEntry>> {
        self.diff_stream()
            .map(TreeDiffEntry::from_backend_entry_with_copies)
            .try_collect()
            .await
    }

    fn into_formatted<F, E>(self, show: F) -> TreeDiffFormatted<F>
    where
        F: Fn(&mut dyn Formatter, &Store, BoxStream<CopiesTreeDiffEntry>) -> Result<(), E>,
        E: Into<TemplatePropertyError>,
    {
        TreeDiffFormatted { diff: self, show }
    }
}

/// Tree diff to be rendered by predefined function `F`.
struct TreeDiffFormatted<F> {
    diff: TreeDiff,
    show: F,
}

impl<F, E> Template for TreeDiffFormatted<F>
where
    F: Fn(&mut dyn Formatter, &Store, BoxStream<CopiesTreeDiffEntry>) -> Result<(), E>,
    E: Into<TemplatePropertyError>,
{
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        let show = &self.show;
        let store = self.diff.from_tree.store();
        let tree_diff = self.diff.diff_stream();
        show(formatter.as_mut(), store, tree_diff).or_else(|err| formatter.handle_error(err.into()))
    }
}

fn builtin_tree_diff_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, TreeDiff> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<TreeDiff>::new();
    map.insert(
        "files",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            // TODO: cache and reuse diff entries within the current evaluation?
            let out_property =
                self_property.and_then(|diff| Ok(diff.collect_entries().block_on()?));
            Ok(L::wrap_tree_diff_entry_list(out_property))
        },
    );
    map.insert(
        "color_words",
        |language, diagnostics, build_ctx, self_property, function| {
            let ([], [context_node]) = function.expect_arguments()?;
            let context_property = context_node
                .map(|node| {
                    template_builder::expect_usize_expression(
                        language,
                        diagnostics,
                        build_ctx,
                        node,
                    )
                })
                .transpose()?;
            let path_converter = language.path_converter;
            let options = diff_util::ColorWordsDiffOptions::from_settings(language.settings())
                .map_err(|err| {
                    let message = "Failed to load diff settings";
                    TemplateParseError::expression(message, function.name_span).with_source(err)
                })?;
            let conflict_marker_style = language.conflict_marker_style;
            let template = (self_property, context_property)
                .map(move |(diff, context)| {
                    let mut options = options.clone();
                    if let Some(context) = context {
                        options.context = context;
                    }
                    diff.into_formatted(move |formatter, store, tree_diff| {
                        diff_util::show_color_words_diff(
                            formatter,
                            store,
                            tree_diff,
                            path_converter,
                            &options,
                            conflict_marker_style,
                        )
                    })
                })
                .into_template();
            Ok(L::wrap_template(template))
        },
    );
    map.insert(
        "git",
        |language, diagnostics, build_ctx, self_property, function| {
            let ([], [context_node]) = function.expect_arguments()?;
            let context_property = context_node
                .map(|node| {
                    template_builder::expect_usize_expression(
                        language,
                        diagnostics,
                        build_ctx,
                        node,
                    )
                })
                .transpose()?;
            let options = diff_util::UnifiedDiffOptions::from_settings(language.settings())
                .map_err(|err| {
                    let message = "Failed to load diff settings";
                    TemplateParseError::expression(message, function.name_span).with_source(err)
                })?;
            let conflict_marker_style = language.conflict_marker_style;
            let template = (self_property, context_property)
                .map(move |(diff, context)| {
                    let mut options = options.clone();
                    if let Some(context) = context {
                        options.context = context;
                    }
                    diff.into_formatted(move |formatter, store, tree_diff| {
                        diff_util::show_git_diff(
                            formatter,
                            store,
                            tree_diff,
                            &options,
                            conflict_marker_style,
                        )
                    })
                })
                .into_template();
            Ok(L::wrap_template(template))
        },
    );
    map.insert(
        "stat",
        |language, diagnostics, build_ctx, self_property, function| {
            let ([], [width_node]) = function.expect_arguments()?;
            let width_property = width_node
                .map(|node| {
                    template_builder::expect_usize_expression(
                        language,
                        diagnostics,
                        build_ctx,
                        node,
                    )
                })
                .transpose()?;
            let path_converter = language.path_converter;
            // No user configuration exists for diff stat.
            let options = diff_util::DiffStatOptions::default();
            let conflict_marker_style = language.conflict_marker_style;
            // TODO: cache and reuse stats within the current evaluation?
            let out_property = (self_property, width_property).and_then(move |(diff, width)| {
                let store = diff.from_tree.store();
                let tree_diff = diff.diff_stream();
                let stats = DiffStats::calculate(store, tree_diff, &options, conflict_marker_style)
                    .block_on()?;
                Ok(DiffStatsFormatted {
                    stats,
                    path_converter,
                    // TODO: fall back to current available width
                    width: width.unwrap_or(80),
                })
            });
            Ok(L::wrap_diff_stats(out_property))
        },
    );
    map.insert(
        "summary",
        |language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let path_converter = language.path_converter;
            let template = self_property
                .map(move |diff| {
                    diff.into_formatted(move |formatter, _store, tree_diff| {
                        diff_util::show_diff_summary(formatter, tree_diff, path_converter)
                    })
                })
                .into_template();
            Ok(L::wrap_template(template))
        },
    );
    // TODO: add support for external tools
    map
}

/// [`MergedTree`] diff entry.
#[derive(Clone, Debug)]
pub struct TreeDiffEntry {
    pub path: CopiesTreeDiffEntryPath,
    pub source_value: MergedTreeValue,
    pub target_value: MergedTreeValue,
}

impl TreeDiffEntry {
    fn from_backend_entry_with_copies(entry: CopiesTreeDiffEntry) -> BackendResult<Self> {
        let (source_value, target_value) = entry.values?;
        Ok(TreeDiffEntry {
            path: entry.path,
            source_value,
            target_value,
        })
    }

    fn status_label(&self) -> &'static str {
        let (label, _sigil) = diff_util::diff_status_label_and_char(
            &self.path,
            &self.source_value,
            &self.target_value,
        );
        label
    }

    fn into_source_entry(self) -> TreeEntry {
        TreeEntry {
            path: self.path.source.map_or(self.path.target, |(path, _)| path),
            value: self.source_value,
        }
    }

    fn into_target_entry(self) -> TreeEntry {
        TreeEntry {
            path: self.path.target,
            value: self.target_value,
        }
    }
}

fn builtin_tree_diff_entry_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, TreeDiffEntry>
{
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<TreeDiffEntry>::new();
    map.insert(
        "path",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|entry| entry.path.target);
            Ok(L::wrap_repo_path(out_property))
        },
    );
    map.insert(
        "status",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|entry| entry.status_label().to_owned());
            Ok(L::wrap_string(out_property))
        },
    );
    // TODO: add status_code() or status_char()?
    map.insert(
        "source",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(TreeDiffEntry::into_source_entry);
            Ok(L::wrap_tree_entry(out_property))
        },
    );
    map.insert(
        "target",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(TreeDiffEntry::into_target_entry);
            Ok(L::wrap_tree_entry(out_property))
        },
    );
    map
}

/// [`MergedTree`] entry.
#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub path: RepoPathBuf,
    pub value: MergedTreeValue,
}

fn builtin_tree_entry_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, TreeEntry> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<TreeEntry>::new();
    map.insert(
        "path",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|entry| entry.path);
            Ok(L::wrap_repo_path(out_property))
        },
    );
    map.insert(
        "conflict",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|entry| !entry.value.is_resolved());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map.insert(
        "file_type",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|entry| describe_file_type(&entry.value).to_owned());
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "executable",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.map(|entry| is_executable_file(&entry.value).unwrap_or_default());
            Ok(L::wrap_boolean(out_property))
        },
    );
    map
}

fn describe_file_type(value: &MergedTreeValue) -> &'static str {
    match value.as_resolved() {
        Some(Some(TreeValue::File { .. })) => "file",
        Some(Some(TreeValue::Symlink(_))) => "symlink",
        Some(Some(TreeValue::Tree(_))) => "tree",
        Some(Some(TreeValue::GitSubmodule(_))) => "git-submodule",
        Some(None) => "", // absent
        None | Some(Some(TreeValue::Conflict(_))) => "conflict",
    }
}

fn is_executable_file(value: &MergedTreeValue) -> Option<bool> {
    let executable = value.to_executable_merge()?;
    conflicts::resolve_file_executable(&executable)
}

/// [`DiffStats`] with rendering parameters.
#[derive(Clone, Debug)]
pub struct DiffStatsFormatted<'a> {
    stats: DiffStats,
    path_converter: &'a RepoPathUiConverter,
    width: usize,
}

impl Template for DiffStatsFormatted<'_> {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        diff_util::show_diff_stats(
            formatter.as_mut(),
            &self.stats,
            self.path_converter,
            self.width,
        )
    }
}

fn builtin_diff_stats_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, DiffStats> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<DiffStats>::new();
    // TODO: add files() -> List<DiffStatEntry> ?
    map.insert(
        "total_added",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.and_then(|stats| Ok(stats.count_total_added().try_into()?));
            Ok(L::wrap_integer(out_property))
        },
    );
    map.insert(
        "total_removed",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property =
                self_property.and_then(|stats| Ok(stats.count_total_removed().try_into()?));
            Ok(L::wrap_integer(out_property))
        },
    );
    map
}

#[derive(Debug)]
pub struct CryptographicSignature {
    commit: Commit,
}

impl CryptographicSignature {
    fn new(commit: Commit) -> Option<Self> {
        commit.is_signed().then_some(Self { commit })
    }

    fn verify(&self) -> SignResult<Verification> {
        self.commit
            .verification()
            .transpose()
            .expect("must have signature")
    }

    fn status(&self) -> SignResult<SigStatus> {
        self.verify().map(|verification| verification.status)
    }

    /// Defaults to empty string if key is not present.
    fn key(&self) -> SignResult<String> {
        self.verify()
            .map(|verification| verification.key.unwrap_or_default())
    }

    /// Defaults to empty string if display is not present.
    fn display(&self) -> SignResult<String> {
        self.verify()
            .map(|verification| verification.display.unwrap_or_default())
    }
}

fn builtin_cryptographic_signature_methods<'repo>(
) -> CommitTemplateBuildMethodFnMap<'repo, CryptographicSignature> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    // Not using maplit::hashmap!{} or custom declarative macro here because
    // code completion inside macro is quite restricted.
    let mut map = CommitTemplateBuildMethodFnMap::<CryptographicSignature>::new();
    map.insert(
        "status",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.and_then(|sig| match sig.status() {
                Ok(status) => Ok(status.to_string()),
                Err(SignError::InvalidSignatureFormat) => Ok("invalid".to_string()),
                Err(err) => Err(err.into()),
            });
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "key",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.and_then(|sig| Ok(sig.key()?));
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "display",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.and_then(|sig| Ok(sig.display()?));
            Ok(L::wrap_string(out_property))
        },
    );
    map
}

#[derive(Debug, Clone)]
pub struct AnnotationLine {
    pub commit: Commit,
    pub content: BString,
    pub line_number: usize,
    pub first_line_in_hunk: bool,
}

fn builtin_annotation_line_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, AnnotationLine>
{
    type L<'repo> = CommitTemplateLanguage<'repo>;
    let mut map = CommitTemplateBuildMethodFnMap::<AnnotationLine>::new();
    map.insert(
        "commit",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|line| line.commit);
            Ok(L::wrap_commit(out_property))
        },
    );
    map.insert(
        "content",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|line| line.content);
            // TODO: Add Bytes or BString template type?
            Ok(L::wrap_template(out_property.into_template()))
        },
    );
    map.insert(
        "line_number",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.and_then(|line| Ok(line.line_number.try_into()?));
            Ok(L::wrap_integer(out_property))
        },
    );
    map.insert(
        "first_line_in_hunk",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|line| line.first_line_in_hunk);
            Ok(L::wrap_boolean(out_property))
        },
    );
    map
}

impl Template for Trailer {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        write!(formatter, "{}: {}", self.key, self.value)
    }
}

impl Template for Vec<Trailer> {
    fn format(&self, formatter: &mut TemplateFormatter) -> io::Result<()> {
        templater::format_joined(formatter, self, "\n")
    }
}

fn builtin_trailer_methods<'repo>() -> CommitTemplateBuildMethodFnMap<'repo, Trailer> {
    type L<'repo> = CommitTemplateLanguage<'repo>;
    let mut map = CommitTemplateBuildMethodFnMap::<Trailer>::new();
    map.insert(
        "key",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|trailer| trailer.key);
            Ok(L::wrap_string(out_property))
        },
    );
    map.insert(
        "value",
        |_language, _diagnostics, _build_ctx, self_property, function| {
            function.expect_no_arguments()?;
            let out_property = self_property.map(|trailer| trailer.value);
            Ok(L::wrap_string(out_property))
        },
    );
    map
}
