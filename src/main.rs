use anyhow::{Context, Result};
use clap::Parser;
use lopdf::{Document, Object, ObjectId};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::time::Instant;

/// PDFを章（トップレベルのブックマーク）ごとに分割するツール
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 入力PDFファイルのパス
    #[arg(help = "分割したいPDFファイルのパスを指定してください")]
    input_path: PathBuf,
}

fn decode_pdf_string(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u16_vec: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        String::from_utf16(&u16_vec).unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
    } else {
        String::from_utf8_lossy(bytes).to_string()
    }
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | '?' | '%' | '*' | ':' | '|' | '"' | '<' | '>' | '.' => '_',
            c if c.is_control() => '_',
            _ => c,
        })
        .collect()
}

fn main() -> Result<()> {
    let args = Args::parse();
    let input_path = args.input_path;

    let file_stem = input_path
        .file_stem()
        .context("Invalid file name")?
        .to_string_lossy()
        .to_string();
    let parent_dir = input_path.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf();

    println!("Loading PDF: {:?}", input_path);
    let load_start = Instant::now();
    
    // ★高速化: BufReaderを使って読み込みをバッファリングする
    let file = File::open(&input_path).with_context(|| format!("Failed to open file: {:?}", input_path))?;
    let reader = BufReader::new(file);
    let doc = Document::load_from(reader)
        .with_context(|| format!("Failed to load PDF: {:?}", input_path))?;
    
    println!("PDF loaded in {:.2?}. Analyzing structure...", load_start.elapsed());

    // 1. ページIDとページ番号の対応表
    let page_numbers = doc.get_pages();
    let object_id_to_page: BTreeMap<_, _> = page_numbers
        .iter()
        .map(|(num, id)| (*id, *num))
        .collect();

    // 2. 名前付き宛先の解決マップ作成
    println!("Building Named Destinations map...");
    let mut named_dests: HashMap<Vec<u8>, Object> = HashMap::new();
    
    if let Ok(catalog_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = doc.get_object(catalog_ref).and_then(|o| o.as_dict()) {
            if let Ok(names_obj) = catalog.get(b"Names") {
                if let Ok(names_real) = resolve_object(&doc, names_obj) {
                    if let Ok(names_dict) = names_real.as_dict() {
                        if let Ok(dests_obj) = names_dict.get(b"Dests") {
                             let dests_real_res = resolve_object(&doc, dests_obj);
                             if let Ok(dests_real) = dests_real_res {
                                 if dests_real.as_dict().is_ok() {
                                     if let Ok(id) = names_dict.get(b"Dests").and_then(|o| o.as_reference()) {
                                         collect_name_tree_recursive(&doc, id, &mut named_dests);
                                     } else if let Ok(dests_dict) = dests_real.as_dict() {
                                         if let Ok(names_arr_obj) = dests_dict.get(b"Names") {
                                             if let Ok(names_arr_real) = resolve_object(&doc, names_arr_obj) {
                                                 if let Ok(names) = names_arr_real.as_array() {
                                                     for chunk in names.chunks(2) {
                                                         if chunk.len() == 2 {
                                                             let key = match &chunk[0] {
                                                                 Object::String(bytes, _) => Some(bytes.clone()),
                                                                 Object::Name(bytes) => Some(bytes.clone()),
                                                                 _ => None,
                                                             };
                                                             if let Some(k) = key {
                                                                 named_dests.insert(k, chunk[1].clone());
                                                             }
                                                         }
                                                     }
                                                 }
                                             }
                                         }
                                     }
                                 }
                             }
                        }
                    }
                }
            }
            if let Ok(dests_obj) = catalog.get(b"Dests") {
                if let Ok(dests_real) = resolve_object(&doc, dests_obj) {
                    if let Ok(dests_dict) = dests_real.as_dict() {
                        for (key, val) in dests_dict.iter() {
                            named_dests.insert(key.clone(), val.clone());
                        }
                    }
                }
            }
        }
    }
    println!("Loaded {} named destinations.", named_dests.len());

    // 3. 目次スキャン
    let mut chapter_starts = Vec::new();
    let mut scan_log = Vec::new();

    if let Ok(catalog_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = doc.get_object(catalog_ref).and_then(|o| o.as_dict()) {
            let outlines_opt = if let Ok(obj) = catalog.get(b"Outlines") {
                 if let Ok(real_obj) = resolve_object(&doc, obj) {
                     real_obj.as_dict().ok()
                 } else { None }
            } else { None };

            if let Some(outlines) = outlines_opt {
                println!("Scanning Outlines (Top-level only)...");
                if let Some(first_ref) = outlines.get(b"First").ok().and_then(|o| o.as_reference().ok()) {
                     collect_bookmarks_top_level(
                         &doc, 
                         first_ref, 
                         &object_id_to_page, 
                         &named_dests, 
                         &mut chapter_starts,
                         &mut scan_log
                    );
                }
            } else {
                println!("PDF has no Outlines dictionary.");
            }
        }
    }

    if chapter_starts.is_empty() {
        println!("警告: 有効な目次が見つかりませんでした。");
        chapter_starts.push((1, "FullDocument".to_string()));
    }

    chapter_starts.sort_by_key(|k| k.0);
    chapter_starts.dedup_by_key(|k| k.0);

    let total_chapters = chapter_starts.len();
    println!("Found {} chapters. Starting parallel processing...", total_chapters);

    let total_pages = page_numbers.len() as u32;

    // 並列処理
    chapter_starts.par_iter().enumerate().for_each(|(i, (start_page, title))| {
        let end_page = if i + 1 < total_chapters {
            if chapter_starts[i + 1].0 > *start_page {
                chapter_starts[i + 1].0 - 1
            } else {
                *start_page
            }
        } else {
            total_pages
        };

        if start_page > &end_page { return; }

        let start_time = Instant::now();
        let mut split_doc = doc.clone();

        let all_pages: Vec<u32> = page_numbers.keys().cloned().collect();
        let mut pages_to_delete = Vec::new();
        for p in all_pages {
            if p < *start_page || p > end_page {
                pages_to_delete.push(p);
            }
        }
        split_doc.delete_pages(&pages_to_delete);
        
        let safe_title = sanitize_filename(title);
        let safe_title_short = if safe_title.chars().count() > 50 {
            safe_title.chars().take(50).collect::<String>()
        } else {
            safe_title
        };

        let out_filename = format!("{}_chapter_{}_{}.pdf", file_stem, i + 1, safe_title_short);
        let out_path = parent_dir.join(&out_filename);

        if let Err(e) = split_doc.save(&out_path) {
            eprintln!("Error saving {}: {:?}", out_filename, e);
        } else {
            println!(
                "Saved: [{}/{} p.{}-p.{}] \"{}\" ({:.2?})", 
                i + 1, total_chapters, start_page, end_page, out_filename, start_time.elapsed()
            );
        }
    });
    
    println!("All Done!");
    Ok(())
}

fn resolve_object<'a>(doc: &'a Document, obj: &'a Object) -> Result<&'a Object, lopdf::Error> {
    match obj {
        Object::Reference(id) => doc.get_object(*id),
        _ => Ok(obj),
    }
}

fn collect_name_tree_recursive(doc: &Document, node_id: ObjectId, map: &mut HashMap<Vec<u8>, Object>) {
    if let Ok(node) = doc.get_object(node_id).and_then(|o| o.as_dict()) {
        if let Ok(names_obj) = node.get(b"Names") {
             if let Ok(names_real) = resolve_object(doc, names_obj) {
                 if let Ok(names) = names_real.as_array() {
                    for chunk in names.chunks(2) {
                        if chunk.len() == 2 {
                            let key = match &chunk[0] {
                                Object::String(bytes, _) => Some(bytes.clone()),
                                Object::Name(bytes) => Some(bytes.clone()),
                                _ => None,
                            };
                            if let Some(k) = key {
                                map.insert(k, chunk[1].clone());
                            }
                        }
                    }
                 }
             }
        }
        if let Ok(kids_obj) = node.get(b"Kids") {
            if let Ok(kids_real) = resolve_object(doc, kids_obj) {
                if let Ok(kids) = kids_real.as_array() {
                    for kid in kids {
                        if let Ok(kid_ref) = kid.as_reference() {
                            collect_name_tree_recursive(doc, kid_ref, map);
                        }
                    }
                }
            }
        }
    }
}

fn collect_bookmarks_top_level(
    doc: &Document,
    start_id: ObjectId,
    object_id_to_page: &BTreeMap<ObjectId, u32>,
    named_dests: &HashMap<Vec<u8>, Object>,
    results: &mut Vec<(u32, String)>,
    log: &mut Vec<String>
) {
    let mut current_id_opt = Some(start_id);
    while let Some(id) = current_id_opt {
        if let Ok(item) = doc.get_object(id).and_then(|o| o.as_dict()) {
            
            let title = item.get(b"Title")
                .ok()
                .and_then(|o| o.as_str().ok())
                .map(|bytes| decode_pdf_string(bytes))
                .unwrap_or_else(|| "No Title".to_string());

            let mut target_page_num = None;
            if let Ok(dest) = item.get(b"Dest") {
                target_page_num = resolve_dest(doc, dest, object_id_to_page, named_dests);
            }
            if target_page_num.is_none() {
                if let Ok(action_obj) = item.get(b"A") {
                    if let Ok(action) = resolve_object(doc, action_obj).and_then(|o| o.as_dict()) {
                         let is_goto = action.get(b"S")
                            .ok()
                            .and_then(|o| o.as_name_str().ok())
                            .map(|s| s == "GoTo")
                            .unwrap_or(false);
                        if is_goto {
                            if let Ok(d) = action.get(b"D") {
                                target_page_num = resolve_dest(doc, d, object_id_to_page, named_dests);
                            }
                        }
                    }
                }
            }
            if let Some(page_num) = target_page_num {
                results.push((page_num, title));
            } else {
                log.push(format!("Skipped: '{}'", title));
            }

            current_id_opt = item.get(b"Next")
                .ok()
                .and_then(|o| o.as_reference().ok());
        } else {
            break;
        }
    }
}

fn resolve_dest(
    doc: &Document,
    dest_obj: &Object, 
    page_map: &BTreeMap<ObjectId, u32>,
    named_dests: &HashMap<Vec<u8>, Object>
) -> Option<u32> {
    let real_dest = match resolve_object(doc, dest_obj) {
        Ok(o) => o, Err(_) => return None,
    };
    if let Ok(arr) = real_dest.as_array() {
        if let Some(Ok(page_ref)) = arr.get(0).map(|o| o.as_reference()) {
            return page_map.get(&page_ref).cloned();
        }
        return None;
    }
    let key = match real_dest {
        Object::String(bytes, _) => Some(bytes.clone()),
        Object::Name(bytes) => Some(bytes.clone()),
        _ => None,
    };
    if let Some(k) = key {
        if let Some(target_obj) = named_dests.get(&k) {
            if let Ok(resolved_target) = resolve_object(doc, target_obj) {
                if let Ok(arr) = resolved_target.as_array() {
                    if let Some(Ok(page_ref)) = arr.get(0).map(|o| o.as_reference()) {
                        return page_map.get(&page_ref).cloned();
                    }
                }
                if let Ok(dict) = resolved_target.as_dict() {
                    if let Ok(inner_d) = dict.get(b"D") {
                         if let Ok(inner_arr_obj) = resolve_object(doc, inner_d) {
                             if let Ok(inner_arr) = inner_arr_obj.as_array() {
                                 if let Some(Ok(page_ref)) = inner_arr.get(0).map(|o| o.as_reference()) {
                                     return page_map.get(&page_ref).cloned();
                                 }
                             }
                         }
                    }
                }
            }
        }
    }
    None
}