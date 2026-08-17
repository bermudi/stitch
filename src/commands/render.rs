use super::common::print_warnings;
use crate::config::{self, Config, Loaded};
use crate::error::StitchError;
use crate::platform::Platform;
use crate::render;
use crate::report;

fn validate_render_spec(
    loaded: &Loaded,
    store_name: &str,
    source_rel: &str,
) -> Result<(), StitchError> {
    if !config::is_safe_fragment(store_name) {
        return Err(StitchError::path_validation(format!(
            "invalid store name '{store_name}': must be relative and contain no '.', '..' or leading '/'"
        )));
    }
    if !config::is_safe_fragment(source_rel) {
        return Err(StitchError::path_validation(format!(
            "invalid source path '{source_rel}': must be relative and contain no '.', '..' or leading '/'"
        )));
    }
    if !loaded.config.stores.contains_key(store_name) {
        let valid: Vec<_> = loaded.config.stores.keys().cloned().collect();
        return Err(StitchError::unknown_store(
            vec![store_name.to_string()],
            valid,
        ));
    }
    Ok(())
}

pub(crate) fn cmd_render(
    root: &std::path::Path,
    spec: &str,
    json: bool,
) -> Result<(), StitchError> {
    let (store_name, source_rel) = spec.split_once('/').ok_or_else(|| {
        StitchError::usage("render: expected <store>/<file>, e.g. git/gitconfig.tmpl")
    })?;
    if source_rel.is_empty() {
        return Err(StitchError::usage("render: missing file name"));
    }
    if !render::is_template(source_rel) {
        return Err(StitchError::usage(
            "render: only .tmpl files can be rendered",
        ));
    }

    if json {
        return report::run_json("render", None, || {
            let loaded =
                Config::load(root).map_err(|e| Box::new((StitchError::from(e), Vec::new())))?;
            validate_render_spec(&loaded, store_name, source_rel)
                .map_err(|e| Box::new((e, loaded.warnings.clone())))?;
            let warnings = loaded.warnings;
            let store_dir = root.join(store_name);
            let source_path = store_dir.join(source_rel);
            if !source_path.is_file() {
                return Err(Box::new((
                    StitchError::internal(format!(
                        "source does not exist: {}",
                        source_path.display()
                    )),
                    warnings,
                )));
            }
            let platform = Platform::detect();
            let content =
                render::render_file(&source_path, source_rel, &platform, &loaded.config.vars)
                    .map_err(|e| {
                        Box::new((StitchError::render(&source_path, e), warnings.clone()))
                    })?;
            let data = report::render(&source_path, source_rel, &content);
            Ok((data, warnings))
        });
    }

    let loaded = Config::load(root)?;
    print_warnings(&loaded);
    validate_render_spec(&loaded, store_name, source_rel)?;
    let store_dir = root.join(store_name);
    let source_path = store_dir.join(source_rel);
    if !source_path.is_file() {
        return Err(StitchError::internal(format!(
            "source does not exist: {}",
            source_path.display()
        )));
    }
    let platform = Platform::detect();
    let content = render::render_file(&source_path, source_rel, &platform, &loaded.config.vars)
        .map_err(|e| StitchError::render(&source_path, e))?;
    print!("{content}");
    Ok(())
}
