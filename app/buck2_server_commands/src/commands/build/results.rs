/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

//! Processing and reporting the the results of the build

use buck2_core::provider::label::ConfiguredProvidersLabel;

pub(crate) enum BuildOwner<'a> {
    Target(&'a ConfiguredProvidersLabel),
}

pub mod result_report {
    use buck2_build_api::build::BuildProviderType;
    use buck2_build_api::build::BuildTargetResult;
    use buck2_build_api::build::ConfiguredBuildTargetResult;
    use buck2_build_api::build::ProviderArtifacts;
    use buck2_core::configuration::compatibility::MaybeCompatible;
    use buck2_core::fs::artifact_path_resolver::ArtifactFs;
    use buck2_error::shared_result::SharedError;
    use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
    use dupe::Dupe;
    use starlark_map::small_map::SmallMap;

    use crate::commands::build::results::BuildOwner;

    mod proto {
        pub use buck2_cli_proto::build_target::build_output::BuildOutputProviders;
        pub use buck2_cli_proto::build_target::BuildOutput;
        pub use buck2_cli_proto::BuildTarget;
    }

    /// Simple container for multiple [`SharedError`]s
    pub(crate) struct BuildErrors {
        pub(crate) errors: Vec<SharedError>,
    }

    #[derive(Copy, Clone, Dupe)]
    pub(crate) struct ResultReporterOptions {
        pub(crate) return_outputs: bool,
        pub(crate) return_default_other_outputs: bool,
    }

    /// Collects build results into a Result<Vec<proto::BuildTarget>, SharedErrors>. If any targets
    /// fail, then the error case will be returned, otherwise a vec of all the successful results.
    pub(crate) struct ResultReporter<'a> {
        artifact_fs: &'a ArtifactFs,
        options: ResultReporterOptions,
        results: Result<Vec<proto::BuildTarget>, BuildErrors>,
    }

    impl<'a> ResultReporter<'a> {
        pub(crate) fn convert(
            artifact_fs: &'a ArtifactFs,
            options: ResultReporterOptions,
            build_result: &BuildTargetResult,
        ) -> Result<Vec<proto::BuildTarget>, BuildErrors> {
            let mut out = Self {
                artifact_fs,
                options,
                results: Ok(Vec::new()),
            };

            let mut non_action_errors = vec![];
            let mut action_errors = vec![];
            non_action_errors.extend(build_result.other_errors.values().flatten().cloned());

            for (k, v) in &build_result.configured {
                // We omit skipped targets here.
                let Some(v) = v else { continue };
                non_action_errors.extend(v.errors.iter().cloned());
                action_errors.extend(v.outputs.iter().filter_map(|x| x.as_ref().err()).cloned());

                let owner = BuildOwner::Target(k);
                out.collect_result(&owner, v);
            }

            if let Some(e) = non_action_errors.pop() {
                return Err(BuildErrors {
                    // FIXME(JakobDegen): We'd like to return more than one error here, but we have
                    // to get better at error deduplication first
                    errors: vec![e],
                });
            }
            if !action_errors.is_empty() {
                return Err(BuildErrors {
                    errors: action_errors,
                });
            }

            out.results
        }

        fn collect_result(&mut self, label: &BuildOwner, result: &ConfiguredBuildTargetResult) {
            let outputs = result
                .outputs
                .iter()
                .filter_map(|output| output.as_ref().ok())
                .collect::<Vec<_>>();

            if let Ok(r) = &mut self.results {
                let artifacts = if self.options.return_outputs {
                    // NOTE: We use an SmallMap here to preserve the order the rule author wrote, all
                    // the while avoiding duplicates.
                    let mut artifacts = SmallMap::new();

                    for output in outputs {
                        let ProviderArtifacts {
                            values,
                            provider_type,
                        } = output;

                        if !self.options.return_default_other_outputs
                            && matches!(provider_type, BuildProviderType::DefaultOther)
                        {
                            continue;
                        }

                        for (artifact, _value) in values.iter() {
                            let entry = artifacts.entry(artifact).or_insert_with(|| {
                                proto::BuildOutputProviders {
                                    default_info: false,
                                    run_info: false,
                                    other: false,
                                    test_info: false,
                                }
                            });

                            match provider_type {
                                BuildProviderType::Default => {
                                    entry.default_info = true;
                                }
                                BuildProviderType::DefaultOther => {
                                    entry.other = true;
                                }
                                BuildProviderType::Run => {
                                    entry.run_info = true;
                                }
                                BuildProviderType::Test => {
                                    entry.test_info = true;
                                }
                            }
                        }
                    }

                    let artifact_fs = &self.artifact_fs;

                    // Write it this way because `.into_iter()` gets rust-analyzer confused
                    IntoIterator::into_iter(artifacts)
                        .map(|(a, providers)| proto::BuildOutput {
                            path: a.resolve_path(artifact_fs).unwrap().to_string(),
                            providers: Some(providers),
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                let (target, configuration) = match label {
                    BuildOwner::Target(t) => (t.unconfigured().to_string(), t.cfg().to_string()),
                };

                let configured_graph_size = match &result.configured_graph_size {
                    Some(Ok(MaybeCompatible::Compatible(v))) => Some(*v),
                    Some(Ok(MaybeCompatible::Incompatible(..))) => None,
                    Some(Err(e)) => {
                        // We don't expect an error on this unless something else on this target
                        // failed.
                        tracing::debug!(
                            "Graph size calculation error failed for {}: {:#}",
                            target,
                            e
                        );
                        None
                    }
                    None => None,
                };

                r.push(proto::BuildTarget {
                    target,
                    configuration,
                    run_args: result.run_args.clone().unwrap_or_default(),
                    outputs: artifacts,
                    configured_graph_size,
                })
            };
        }
    }
}

pub mod build_report {
    use std::collections::HashMap;

    use buck2_build_api::build::BuildProviderType;
    use buck2_build_api::build::ConfiguredBuildTargetResult;
    use buck2_core::configuration::compatibility::MaybeCompatible;
    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::fs::artifact_path_resolver::ArtifactFs;
    use buck2_core::fs::paths::abs_norm_path::AbsNormPathBuf;
    use buck2_core::fs::project::ProjectRoot;
    use buck2_core::fs::project_rel_path::ProjectRelativePathBuf;
    use buck2_core::provider::label::NonDefaultProvidersName;
    use buck2_core::provider::label::ProvidersLabel;
    use buck2_core::provider::label::ProvidersName;
    use buck2_core::target::label::TargetLabel;
    use buck2_execute::artifact::artifact_dyn::ArtifactDyn;
    use buck2_wrapper_common::invocation_id::TraceId;
    use derivative::Derivative;
    use dupe::Dupe;
    use itertools::Itertools;
    use serde::Serialize;
    use starlark_map::small_set::SmallSet;

    use crate::commands::build::results::BuildOwner;

    #[derive(Debug, Serialize)]
    #[allow(clippy::upper_case_acronyms)] // We care about how they serialise
    enum BuildOutcome {
        SUCCESS,
        FAIL,
        #[allow(dead_code)] // Part of the spec, but not yet used
        CANCELED,
    }

    impl Default for BuildOutcome {
        fn default() -> Self {
            Self::SUCCESS
        }
    }

    #[derive(Debug, Serialize)]
    pub(crate) struct BuildReport {
        trace_id: TraceId,
        success: bool,
        results: HashMap<EntryLabel, BuildReportEntry>,
        failures: HashMap<EntryLabel, ProjectRelativePathBuf>,
        project_root: AbsNormPathBuf,
        truncated: bool,
    }

    #[derive(Default, Debug, Serialize)]
    struct ConfiguredBuildReportEntry {
        /// whether this particular target was successful
        success: BuildOutcome,
        /// a map of each subtarget of the current target (outputted as a `|` delimited list) to
        /// the default exposed output of the subtarget
        outputs: HashMap<String, Vec<ProjectRelativePathBuf>>,
        /// a map of each subtarget of the current target (outputted as a `|` delimited list) to
        /// the hidden, implicitly built outputs of the subtarget. There are multiple outputs
        /// per subtarget
        other_outputs: HashMap<String, Vec<ProjectRelativePathBuf>>,
        /// The size of the graph for this target, if it was produced
        configured_graph_size: Option<u64>,
    }

    #[derive(Default, Debug, Serialize)]
    pub(crate) struct ConfiguredBuildReportEntryWithErrors {
        /// A list of errors that occurred while building this target
        errors: Vec<BuildReportError>,
        #[serde(flatten)]
        inner: ConfiguredBuildReportEntry,
    }

    #[derive(Debug, Serialize)]
    struct BuildReportEntry {
        /// The buck1 build report did not support multiple configurations of the same target. We
        /// do, which is why we have the `configured` field below, which users should ideally use.
        /// This field is kept around for buck1 compatibility only and should ideally be removed.
        ///
        /// We avoid the `WithErrors` variant here, to keep the errors field from conflicting with
        /// the one on this struct.
        #[serde(flatten)]
        #[serde(skip_serializing_if = "Option::is_none")]
        compatible: Option<ConfiguredBuildReportEntry>,

        /// the configured entry
        configured: HashMap<ConfigurationData, ConfiguredBuildReportEntryWithErrors>,

        /// Errors that could not be associated with a particular configured version of the target,
        /// typically because they happened before configuration.
        errors: Vec<BuildReportError>,
    }

    #[derive(Debug, Clone, Serialize, PartialOrd, Ord, PartialEq, Eq)]
    struct BuildReportError {
        message: String,
    }

    #[derive(Derivative, Serialize, Eq, PartialEq, Hash)]
    #[derivative(Debug)]
    #[serde(untagged)]
    enum EntryLabel {
        #[derivative(Debug = "transparent")]
        Target(TargetLabel),
    }

    /// Collects BuildEvents to form a BuildReport.
    pub(crate) struct BuildReportCollector<'a> {
        trace_id: &'a TraceId,
        artifact_fs: &'a ArtifactFs,
        build_report_results: HashMap<EntryLabel, BuildReportEntry>,
        overall_success: bool,
        project_root: &'a ProjectRoot,
        include_unconfigured_section: bool,
        include_other_outputs: bool,
    }

    impl<'a> BuildReportCollector<'a> {
        pub(crate) fn new(
            trace_id: &'a TraceId,
            artifact_fs: &'a ArtifactFs,
            project_root: &'a ProjectRoot,
            include_unconfigured_section: bool,
            include_other_outputs: bool,
        ) -> Self {
            Self {
                trace_id,
                artifact_fs,
                build_report_results: HashMap::new(),
                overall_success: true,
                project_root,
                include_unconfigured_section,
                include_other_outputs,
            }
        }

        pub(crate) fn into_report(self) -> BuildReport {
            BuildReport {
                trace_id: self.trace_id.dupe(),
                success: self.overall_success,
                results: self.build_report_results,
                failures: HashMap::new(),
                project_root: self.project_root.root().to_owned(),
                // In buck1 we may truncate build report for a large number of targets.
                // Setting this to false since we don't currently truncate buck2's build report.
                truncated: false,
            }
        }

        pub(crate) fn collect_result(
            &mut self,
            label: &BuildOwner,
            result: &ConfiguredBuildTargetResult,
        ) {
            let (default_outs, other_outs, mut errors) = {
                let mut default_outs = SmallSet::new();
                let mut other_outs = SmallSet::new();
                let mut errors = Vec::new();

                result.outputs.iter().for_each(|res| {
                    match res {
                        Ok(artifacts) => {
                            let mut is_default = false;
                            let mut is_other = false;

                            match artifacts.provider_type {
                                BuildProviderType::Default => {
                                    // as long as we have requested it as a default info, it should  be
                                    // considered a default output whether or not it also appears as an other
                                    // non-main output
                                    is_default = true;
                                }
                                BuildProviderType::DefaultOther
                                | BuildProviderType::Run
                                | BuildProviderType::Test => {
                                    // as long as the output isn't the default, we add it to other outputs.
                                    // This means that the same artifact may appear twice if its part of the
                                    // default AND the other outputs, but this is intended as it accurately
                                    // describes the type of the artifact
                                    is_other = true;
                                }
                            }

                            for (artifact, _value) in artifacts.values.iter() {
                                if is_default {
                                    default_outs
                                        .insert(artifact.resolve_path(self.artifact_fs).unwrap());
                                }

                                if is_other && self.include_other_outputs {
                                    other_outs
                                        .insert(artifact.resolve_path(self.artifact_fs).unwrap());
                                }
                            }
                        }
                        Err(e) => {
                            errors.push(BuildReportError {
                                message: format!("{:#}", e),
                            });
                        }
                    }
                });

                (default_outs, other_outs, errors)
            };

            for err in &result.errors {
                errors.push(BuildReportError {
                    message: format!("{:#}", err),
                });
            }

            let report_results = self
                .build_report_results
                .entry(match label {
                    BuildOwner::Target(t) => EntryLabel::Target(t.unconfigured().target().dupe()),
                })
                .or_insert_with(|| BuildReportEntry {
                    compatible: if self.include_unconfigured_section {
                        Some(ConfiguredBuildReportEntry::default())
                    } else {
                        None
                    },
                    configured: HashMap::new(),
                    errors: Vec::new(),
                });

            let unconfigured_report = &mut report_results.compatible;
            let configured_report = report_results
                .configured
                .entry(match label {
                    BuildOwner::Target(t) => t.cfg().dupe(),
                })
                .or_insert(ConfiguredBuildReportEntryWithErrors::default());
            if !default_outs.is_empty() {
                if let Some(report) = unconfigured_report {
                    report.outputs.insert(
                        report_providers_name(label),
                        default_outs.iter().cloned().collect(),
                    );
                }

                configured_report.inner.outputs.insert(
                    report_providers_name(label),
                    default_outs.into_iter().collect(),
                );
            }
            if !other_outs.is_empty() {
                if let Some(report) = unconfigured_report {
                    report.other_outputs.insert(
                        report_providers_name(label),
                        other_outs.iter().cloned().collect(),
                    );
                }

                configured_report.inner.other_outputs.insert(
                    report_providers_name(label),
                    other_outs.into_iter().collect(),
                );
            }

            if !errors.is_empty() {
                if let Some(unconfigured_report) = unconfigured_report {
                    unconfigured_report.success = BuildOutcome::FAIL;
                }
                configured_report.inner.success = BuildOutcome::FAIL;
                configured_report.errors.extend(errors);
                // Keep the output deterministic
                configured_report.errors.sort_unstable();

                self.overall_success = false;
            }

            if let Some(Ok(MaybeCompatible::Compatible(configured_graph_size))) =
                result.configured_graph_size
            {
                if let Some(report) = unconfigured_report {
                    report.configured_graph_size = Some(configured_graph_size);
                }
                configured_report.inner.configured_graph_size = Some(configured_graph_size);
            }
        }

        pub(crate) fn handle_error(&mut self, p: &Option<ProvidersLabel>, e: &buck2_error::Error) {
            self.overall_success = false;
            let Some(p) = p else {
                // We have nowhere in the build report to put this error
                return;
            };
            let target = p.target().dupe();
            let entry = self
                .build_report_results
                .entry(EntryLabel::Target(target))
                .or_insert(BuildReportEntry {
                    compatible: if self.include_unconfigured_section {
                        Some(ConfiguredBuildReportEntry::default())
                    } else {
                        None
                    },
                    configured: HashMap::new(),
                    errors: Vec::new(),
                });
            entry.errors.push(BuildReportError {
                message: format!("{:#}", e),
            });
            if let Some(unconfigured) = entry.compatible.as_mut() {
                unconfigured.success = BuildOutcome::FAIL;
            }
        }
    }

    fn report_providers_name(label: &BuildOwner) -> String {
        match label {
            BuildOwner::Target(t) => match t.name() {
                ProvidersName::Default => "DEFAULT".to_owned(),
                ProvidersName::NonDefault(box NonDefaultProvidersName::Named(names)) => {
                    names.iter().join("|")
                }
                ProvidersName::NonDefault(box NonDefaultProvidersName::UnrecognizedFlavor(f)) => {
                    format!("#{}", f)
                }
            },
        }
    }
}
