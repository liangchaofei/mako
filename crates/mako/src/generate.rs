use std::collections::HashSet;
use std::fs;
use std::time::Instant;

use anyhow::Result;
use rayon::prelude::*;
use serde::Serialize;
use tracing::{debug, info};

use crate::ast::{css_ast_to_code, js_ast_to_code};
use crate::compiler::Compiler;
use crate::config::{DevtoolConfig, Mode};
use crate::generate_chunks::OutputAst;
use crate::minify::minify_js;
use crate::module::ModuleAst;
use crate::update::UpdateResult;

impl Compiler {
    pub fn generate(&self) -> Result<()> {
        info!("generate");
        let t_generate = Instant::now();
        let t_tree_shaking = Instant::now();
        if matches!(self.context.config.mode, Mode::Production) {
            info!("tree_shaking");
            self.tree_shaking();
        }
        let t_tree_shaking = t_tree_shaking.elapsed();
        let t_group_chunks = Instant::now();
        self.group_chunk();
        let t_group_chunks = t_group_chunks.elapsed();

        // 为啥单独提前 transform modules？
        // 因为放 chunks 的循环里，一个 module 可能存在于多个 chunk 里，可能会被编译多遍
        let t_transform_modules = Instant::now();
        info!("transform all modules");
        self.transform_all()?;
        let t_transform_modules = t_transform_modules.elapsed();

        // ensure output dir exists
        let config = &self.context.config;
        if !config.output.path.exists() {
            fs::create_dir_all(&config.output.path)?;
        }

        // generate chunks
        // TODO: 并行
        let t_generate_chunks = Instant::now();
        info!("generate chunks");
        let mut chunk_asts = self.generate_chunks_ast()?;
        let t_generate_chunks = t_generate_chunks.elapsed();

        // minify
        let t_minify = Instant::now();
        info!("minify");
        if self.context.config.minify {
            chunk_asts
                .par_iter_mut()
                .try_for_each(|file| -> Result<()> {
                    if matches!(self.context.config.mode, Mode::Production) {
                        match &mut file.ast {
                            ModuleAst::Script(ast) => {
                                minify_js(ast, &self.context)?;
                            }
                            ModuleAst::Css(ast) => {
                                swc_css_minifier::minify(ast, Default::default());
                            }
                            _ => (),
                        }
                    }
                    Ok(())
                })?;
        }
        let t_minify = t_minify.elapsed();

        // ast to code and sourcemap, then write
        let t_ast_to_code_and_write = Instant::now();
        info!("ast to code and write");
        chunk_asts.par_iter().try_for_each(|file| -> Result<()> {
            match &file.ast {
                ModuleAst::Script(ast) => {
                    // ast to code
                    let (js_code, js_sourcemap) =
                        js_ast_to_code(&ast.ast, &self.context, &file.path)?;
                    // generate code and sourcemap files
                    let output = &config.output.path.join(&file.path);
                    fs::write(output, js_code).unwrap();
                    if matches!(self.context.config.devtool, DevtoolConfig::SourceMap) {
                        fs::write(format!("{}.map", output.display()), js_sourcemap).unwrap();
                    }
                }
                // TODO: Sourcemap part
                ModuleAst::Css(ast) => {
                    // ast to code
                    let (css_code, _sourcemap) = css_ast_to_code(ast, &self.context);
                    let output = &config.output.path.join(&file.path);
                    fs::write(output, css_code).unwrap();
                }
                _ => (),
            }
            Ok(())
        })?;
        let t_ast_to_code_and_write = t_ast_to_code_and_write.elapsed();

        // write assets
        let t_write_assets = Instant::now();
        info!("write assets");
        let assets_info = &(*self.context.assets_info.lock().unwrap());
        for (k, v) in assets_info {
            let asset_path = &self.context.root.join(k);
            let asset_output_path = &config.output.path.join(v);
            if asset_path.exists() {
                fs::copy(asset_path, asset_output_path)?;
            } else {
                panic!("asset not found: {}", asset_path.display());
            }
        }
        let t_write_assets = t_write_assets.elapsed();

        // copy
        let t_copy = Instant::now();
        info!("copy");
        self.copy()?;
        let t_copy = t_copy.elapsed();

        info!("generate done in {}ms", t_generate.elapsed().as_millis());
        info!("  - tree shaking: {}ms", t_tree_shaking.as_millis());
        info!("  - group chunks: {}ms", t_group_chunks.as_millis());
        info!(
            "  - transform modules: {}ms",
            t_transform_modules.as_millis()
        );
        info!("  - generate chunks: {}ms", t_generate_chunks.as_millis());
        info!("  - minify: {}ms", t_minify.as_millis());
        info!(
            "  - ast to code and write: {}ms",
            t_ast_to_code_and_write.as_millis()
        );
        info!("  - write assets: {}ms", t_write_assets.as_millis());
        info!("  - copy: {}ms", t_copy.as_millis());

        Ok(())
    }

    pub fn emit_dev_chunks(&self, chunk_asts: Vec<OutputAst>) -> Result<()> {
        info!("generate(hmr-rebuild)");

        let t_generate_chunks = Instant::now();

        // ensure output dir exists
        let config = &self.context.config;
        if !config.output.path.exists() {
            fs::create_dir_all(&config.output.path)?;
        }

        // ast to code and sourcemap, then write
        let t_ast_to_code_and_write = Instant::now();
        info!("ast to code and write");
        chunk_asts.par_iter().try_for_each(|file| -> Result<()> {
            match &file.ast {
                ModuleAst::Script(ast) => {
                    // ast to code
                    let (js_code, js_sourcemap) =
                        js_ast_to_code(&ast.ast, &self.context, &file.path)?;
                    // generate code and sourcemap files
                    let output = &config.output.path.join(&file.path);
                    fs::write(output, js_code).unwrap();
                    if matches!(self.context.config.devtool, DevtoolConfig::SourceMap) {
                        fs::write(format!("{}.map", output.display()), js_sourcemap).unwrap();
                    }
                }
                ModuleAst::Css(_ast) => {
                    // TODO: css chunk
                }
                _ => (),
            }
            Ok(())
        })?;
        let t_ast_to_code_and_write = t_ast_to_code_and_write.elapsed();

        // write assets
        let t_write_assets = Instant::now();
        info!("write assets");
        let assets_info = &(*self.context.assets_info.lock().unwrap());
        for (k, v) in assets_info {
            let asset_path = &self.context.root.join(k);
            let asset_output_path = &config.output.path.join(v);
            if asset_path.exists() {
                fs::copy(asset_path, asset_output_path)?;
            } else {
                panic!("asset not found: {}", asset_path.display());
            }
        }
        let t_write_assets = t_write_assets.elapsed();

        // copy
        let t_copy = Instant::now();
        info!("copy");
        self.copy()?;
        let t_copy = t_copy.elapsed();

        let t_generate_chunks = t_generate_chunks.elapsed();

        info!(
            "  - generate chunks(hmr): {}ms",
            t_generate_chunks.as_millis()
        );
        info!(
            "  - ast to code and write: {}ms",
            t_ast_to_code_and_write.as_millis()
        );
        info!("  - write assets: {}ms", t_write_assets.as_millis());
        info!("  - copy: {}ms", t_copy.as_millis());

        Ok(())
    }

    // TODO: 集成到 fn generate 里
    pub fn generate_hot_update_chunks(
        &self,
        updated_modules: UpdateResult,
        last_full_hash: u64,
    ) -> Result<u64> {
        info!("generate_hot_update_chunks start");

        let last_chunk_names: HashSet<String> = {
            let chunk_graph = self.context.chunk_graph.read().unwrap();
            chunk_graph.chunk_names()
        };

        info!("hot-update:generate");

        let t_generate = Instant::now();
        let t_group_chunks = Instant::now();
        // TODO 不需要重新构建 graph
        self.group_chunk();
        let t_group_chunks = t_group_chunks.elapsed();

        // 为啥单独提前 transform modules？

        // 因为放 chunks 的循环里，一个 module 可能存在于多个 chunk 里，可能会被编译多遍，
        let t_transform_modules = Instant::now();
        self.transform_all()?;
        let t_transform_modules = t_transform_modules.elapsed();

        let current_full_hash = self.full_hash();

        debug!(
            "{} {} {}",
            current_full_hash,
            if current_full_hash == last_full_hash {
                "equals"
            } else {
                "not equals"
            },
            last_full_hash
        );

        if current_full_hash == last_full_hash {
            return Ok(current_full_hash);
        }

        // ensure output dir exists
        let config = &self.context.config;
        if !config.output.path.exists() {
            fs::create_dir_all(&config.output.path).unwrap();
        }

        let (current_chunks, modified_chunks) = {
            let cg = self.context.chunk_graph.read().unwrap();

            let chunk_names = cg.chunk_names();

            let modified_chunks: Vec<String> = cg
                .get_chunks()
                .iter()
                .filter(|c| {
                    updated_modules
                        .modified
                        .iter()
                        .any(|m_id| c.has_module(m_id))
                })
                .map(|c| c.filename())
                .collect();

            (chunk_names, modified_chunks)
        };

        let removed_chunks: Vec<String> = last_chunk_names
            .difference(&current_chunks)
            .cloned()
            .collect();

        let cg = self.context.chunk_graph.read().unwrap();
        for chunk_name in &modified_chunks {
            if let Some(chunk) = cg.get_chunk_by_name(chunk_name) {
                let (code, ..) =
                    self.generate_hmr_chunk(chunk, &updated_modules.modified, current_full_hash)?;

                // TODO the final format should be {name}.{full_hash}.hot-update.{ext}
                self.write_to_dist(to_hot_update_chunk_name(chunk_name, last_full_hash), code);
            }
        }

        self.write_to_dist(
            format!("{}.hot-update.json", last_full_hash),
            serde_json::to_string(&HotUpdateManifest {
                removed_chunks,
                modified_chunks,
            })
            .unwrap(),
        );

        info!(
            "generate(hmr) done in {}ms",
            t_generate.elapsed().as_millis()
        );
        info!("  - group chunks: {}ms", t_group_chunks.as_millis());
        info!(
            "  - transform modules: {}ms",
            t_transform_modules.as_millis()
        );
        info!("  - next full hash: {}", current_full_hash);

        Ok(current_full_hash)
    }

    pub fn write_to_dist<P: AsRef<std::path::Path>, C: AsRef<[u8]>>(
        &self,
        filename: P,
        content: C,
    ) {
        let to = self.context.config.output.path.join(filename);

        std::fs::write(to, content).unwrap();
    }
}

fn to_hot_update_chunk_name(chunk_name: &String, hash: u64) -> String {
    match chunk_name.rsplit_once('.') {
        None => {
            format!("{chunk_name}.{hash}.hot-update")
        }
        Some((left, ext)) => {
            format!("{left}.{hash}.hot-update.{ext}")
        }
    }
}

#[derive(Serialize)]
struct HotUpdateManifest {
    #[serde(rename(serialize = "c"))]
    modified_chunks: Vec<String>,

    #[serde(rename(serialize = "r"))]
    removed_chunks: Vec<String>,
    // TODO
    // #[serde(rename(serialize = "c"))]
    // removed_modules: Vec<String>,
}
