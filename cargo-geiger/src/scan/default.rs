mod table;

use crate::args::Args;
use crate::format::print_config::OutputFormat;
use crate::graph::Graph;
use crate::krates_utils::CargoMetadataParameters;
use crate::rs_file::resolve_rs_file_deps;

use super::find::find_unsafe;
use super::{
    list_files_used_but_not_scanned, package_metrics, unsafe_stats,
    ScanDetails, ScanMode, ScanParameters,
};

use table::scan_to_table;

use cargo::core::compiler::CompileMode;
use cargo::core::{PackageId, PackageSet, Workspace};
use cargo::ops::CompileOptions;
use cargo::{CliError, CliResult, Config};
use cargo_geiger_serde::{ReportEntry, SafetyReport};

pub fn scan_unsafe(
    cargo_metadata_parameters: &CargoMetadataParameters,
    graph: &Graph,
    package_set: &PackageSet,
    root_package_id: PackageId,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> CliResult {
    match scan_parameters.args.output_format {
        Some(output_format) => scan_to_report(
            cargo_metadata_parameters,
            graph,
            output_format,
            package_set,
            root_package_id,
            scan_parameters,
            workspace,
        ),
        None => scan_to_table(
            cargo_metadata_parameters,
            graph,
            package_set,
            root_package_id,
            scan_parameters,
            workspace,
        ),
    }
}

/// Based on code from cargo-bloat. It seems weird that CompileOptions can be
/// constructed without providing all standard cargo options, TODO: Open an issue
/// in cargo?
fn build_compile_options<'a>(
    args: &'a Args,
    config: &'a Config,
) -> CompileOptions {
    let features = args
        .features
        .as_ref()
        .cloned()
        .unwrap_or_else(String::new)
        .split(' ')
        .map(str::to_owned)
        .collect::<Vec<String>>();
    let mut compile_options =
        CompileOptions::new(&config, CompileMode::Check { test: false })
            .unwrap();
    compile_options.features = features;
    compile_options.all_features = args.all_features;
    compile_options.no_default_features = args.no_default_features;

    // TODO: Investigate if this is relevant to cargo-geiger.
    //let mut bins = Vec::new();
    //let mut examples = Vec::new();
    // opt.release = args.release;
    // opt.target = args.target.clone();
    // if let Some(ref name) = args.bin {
    //     bins.push(name.clone());
    // } else if let Some(ref name) = args.example {
    //     examples.push(name.clone());
    // }
    // if args.bin.is_some() || args.example.is_some() {
    //     opt.filter = ops::CompileFilter::new(
    //         false,
    //         bins.clone(), false,
    //         Vec::new(), false,
    //         examples.clone(), false,
    //         Vec::new(), false,
    //         false,
    //     );
    // }

    compile_options
}

fn scan(
    cargo_metadata_parameters: &CargoMetadataParameters,
    package_set: &PackageSet,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> Result<ScanDetails, CliError> {
    let compile_options =
        build_compile_options(scan_parameters.args, scan_parameters.config);
    let rs_files_used =
        resolve_rs_file_deps(&compile_options, workspace).unwrap();
    let geiger_context = find_unsafe(
        cargo_metadata_parameters,
        scan_parameters.config,
        ScanMode::Full,
        package_set,
        scan_parameters.print_config,
    )?;
    Ok(ScanDetails {
        rs_files_used,
        geiger_context,
    })
}

fn scan_to_report(
    cargo_metadata_parameters: &CargoMetadataParameters,
    graph: &Graph,
    output_format: OutputFormat,
    package_set: &PackageSet,
    root_package_id: PackageId,
    scan_parameters: &ScanParameters,
    workspace: &Workspace,
) -> CliResult {
    let ScanDetails {
        rs_files_used,
        geiger_context,
    } = scan(
        cargo_metadata_parameters,
        package_set,
        scan_parameters,
        workspace,
    )?;
    let mut report = SafetyReport::default();
    for (package, package_metrics_option) in
        package_metrics(&geiger_context, graph, root_package_id)
    {
        let package_metrics = match package_metrics_option {
            Some(m) => m,
            None => {
                report.packages_without_metrics.insert(package.id);
                continue;
            }
        };
        let unsafe_info = unsafe_stats(package_metrics, &rs_files_used);
        let entry = ReportEntry {
            package,
            unsafety: unsafe_info,
        };
        report.packages.insert(entry.package.id.clone(), entry);
    }
    report.used_but_not_scanned_files =
        list_files_used_but_not_scanned(&geiger_context, &rs_files_used)
            .into_iter()
            .collect();
    let s = match output_format {
        OutputFormat::Json => serde_json::to_string(&report).unwrap(),
    };
    println!("{}", s);
    Ok(())
}

#[cfg(test)]
mod default_tests {
    use super::*;
    use crate::format::Charset;
    use rstest::*;

    #[rstest(
        input_features,
        expected_compile_features,
        case(
            Some(String::from("unit test features")),
            vec!["unit", "test", "features"],
        ),
        case(
            Some(String::from("")),
            vec![""],
        )
    )]
    fn build_compile_options_test(
        input_features: Option<String>,
        expected_compile_features: Vec<&str>,
    ) {
        let mut args = create_args();
        args.all_features = rand::random();
        args.features = input_features;
        args.no_default_features = rand::random();

        let config = Config::default().unwrap();
        let compile_options = build_compile_options(&args, &config);

        assert_eq!(compile_options.all_features, args.all_features);
        assert_eq!(compile_options.features, expected_compile_features);
        assert_eq!(
            compile_options.no_default_features,
            args.no_default_features
        );
    }

    fn create_args() -> Args {
        Args {
            all: false,
            all_deps: false,
            all_features: false,
            all_targets: false,
            build_deps: false,
            charset: Charset::Utf8,
            color: None,
            dev_deps: false,
            features: None,
            forbid_only: false,
            format: "".to_string(),
            frozen: false,
            help: false,
            include_tests: false,
            invert: false,
            locked: false,
            manifest_path: None,
            no_default_features: false,
            no_indent: false,
            offline: false,
            package: None,
            prefix_depth: false,
            quiet: false,
            target: None,
            unstable_flags: vec![],
            verbose: 0,
            version: false,
            output_format: None,
        }
    }
}
