use anyhow::{Context, Result};
use clap::Parser;
use lopdf::{Document, Object, ObjectId};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// PDFを章（ブックマーク）ごとに分割するツール
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 入力PDFファイルのパス
    #[arg(help = "分割したいPDFファイルのパスを指定してください")]
    input_path: PathBuf,
}

/// PDFの文字列（バイト列）をRustのStringに変換
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

/// ファイル名に使えない文字を置換
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
        .to_string_lossy();
    let parent_dir = input_path.parent().unwrap_or_else(|| std::path::Path::new("."));

    println!("Loading PDF: {:?}", input_path);
    let mut doc = Document::load(&input_path)
        .with_context(|| format!("Failed to load PDF: {:?}", input_path))?;

    // 1. ページIDとページ番号の対応表
    let page_numbers = doc.get_pages();
    let object_id_to_page: BTreeMap<_, _> = page_numbers
        .iter()
        .map(|(num, id)| (*id, *num))
        .collect();

    // 2. 名前付き宛先 (Named Destinations) の解決マップを作成
    println!("Building Named Destinations map...");
    let mut named_dests: HashMap<Vec<u8>, Object> = HashMap::new();
    
    // Root -> Names -> Dests のツリーを探索
    if let Ok(catalog_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = doc.get_object(catalog_ref).and_then(|o| o.as_dict()) {
            // "Names" 辞書
            if let Ok(names_dict) = catalog.get(b"Names").and_then(|o| resolve_object(&doc, o).and_then(|obj| obj.as_dict())) {
                // "Dests" ツリー
                if let Ok(dests_obj) = names_dict.get(b"Dests").and_then(|o| resolve_object(&doc, o)) {
                     // Destsの実体が辞書(Root Node)なら探索開始
                     if let Ok(dests_root) = dests_obj.as_dict() {
                         // Dests自体がReferenceで指されている場合、そのIDを使って探索を開始
                         // (辞書オブジェクト自体からはIDが取れないため、簡易的にnames_dictから再取得)
                         if let Ok(id) = names_dict.get(b"Dests").and_then(|o| o.as_reference()) {
                             collect_name_tree_recursive(&doc, id, &mut named_dests);
                         }
                     }
                }
            }
            // 互換性: Catalog直下のDests
            if let Ok(dests_dict) = catalog.get(b"Dests").and_then(|o| resolve_object(&doc, o).and_then(|obj| obj.as_dict())) {
                for (key, val) in dests_dict.iter() {
                    named_dests.insert(key.clone(), val.clone());
                }
            }
        }
    }
    println!("Loaded {} named destinations.", named_dests.len());

    // 3. 目次（Outlines）のスキャン
    let mut chapter_starts = Vec::new();
    let mut scan_log = Vec::new();

    if let Ok(catalog_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = doc.get_object(catalog_ref).and_then(|o| o.as_dict()) {
            
            let outlines_obj = match catalog.get(b"Outlines") {
                Ok(Object::Reference(r)) => doc.get_object(*r).ok(),
                Ok(Object::Dictionary(_)) => Some(catalog.get(b"Outlines").unwrap()),
                _ => None,
            };

            if let Some(outlines) = outlines_obj.and_then(|o| o.as_dict().ok()) {
                println!("Scanning Outlines (Recursive)...");
                if let Some(first_ref) = outlines.get(b"First").ok().and_then(|o| o.as_reference().ok()) {
                     collect_bookmarks_recursive(
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

    // デバッグ情報
    if chapter_starts.is_empty() && !scan_log.is_empty() {
        println!("Details of scanned items (for debugging):");
        for log in scan_log.iter().take(20) {
            println!("  {}", log);
        }
    }

    if chapter_starts.is_empty() {
        println!("警告: 有効な目次が見つかりませんでした。全ページを1つのファイルとして出力します。");
        chapter_starts.push((1, "FullDocument".to_string()));
    }

    chapter_starts.sort_by_key(|k| k.0);
    chapter_starts.dedup_by_key(|k| k.0);

    println!("Found {} chapters/sections.", chapter_starts.len());

    let total_pages = page_numbers.len() as u32;

    for (i, (start_page, title)) in chapter_starts.iter().enumerate() {
        let end_page = if i + 1 < chapter_starts.len() {
            if chapter_starts[i + 1].0 > *start_page {
                chapter_starts[i + 1].0 - 1
            } else {
                *start_page
            }
        } else {
            total_pages
        };

        if start_page > &end_page { continue; }

        println!("  Processing: \"{}\" (p.{} - p.{})", title, start_page, end_page);

        let mut split_doc = doc.clone();
        let all_pages: Vec<u32> = page_numbers.keys().cloned().collect();
        let mut pages_to_delete = Vec::new();
        
        for p in all_pages {
            if p < *start_page || p > end_page {
                pages_to_delete.push(p);
            }
        }
        
        split_doc.delete_pages(&pages_to_delete);
        split_doc.prune_objects();

        let safe_title = sanitize_filename(title);
        let safe_title_short = if safe_title.chars().count() > 60 {
            safe_title.chars().take(60).collect::<String>()
        } else {
            safe_title
        };

        let out_filename = format!("{}_chapter_{}_{}.pdf", file_stem, i + 1, safe_title_short);
        let out_path = parent_dir.join(out_filename);

        split_doc.save(&out_path)?;
    }
    
    println!("Done!");
    Ok(())
}

/// オブジェクトがReferenceなら実体を取得し、そうでなければそのまま返すヘルパー
fn resolve_object<'a>(doc: &'a Document, obj: &'a Object) -> Result<&'a Object, lopdf::Error> {
    match obj {
        Object::Reference(id) => doc.get_object(*id),
        _ => Ok(obj),
    }
}

/// NameTree探索
fn collect_name_tree_recursive(doc: &Document, node_id: ObjectId, map: &mut HashMap<Vec<u8>, Object>) {
    if let Ok(node) = doc.get_object(node_id).and_then(|o| o.as_dict()) {
        if let Ok(names) = node.get(b"Names").and_then(|o| resolve_object(doc, o)).and_then(|o| o.as_array()) {
            for chunk in names.chunks(2) {
                if chunk.len() == 2 {
                    // キーは文字列(String)または名前(Name)
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
        if let Ok(kids) = node.get(b"Kids").and_then(|o| resolve_object(doc, o)).and_then(|o| o.as_array()) {
            for kid in kids {
                if let Ok(kid_ref) = kid.as_reference() {
                    collect_name_tree_recursive(doc, kid_ref, map);
                }
            }
        }
    }
}

/// ブックマーク探索
fn collect_bookmarks_recursive(
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

            // Dest
            if let Ok(dest) = item.get(b"Dest") {
                // ★修正点: docを渡してReferenceを解決する
                target_page_num = resolve_dest(doc, dest, object_id_to_page, named_dests);
            }

            // Action (GoTo)
            if target_page_num.is_none() {
                if let Ok(action) = item.get(b"A").and_then(|o| resolve_object(doc, o)).and_then(|o| o.as_dict()) {
                    let is_goto = action.get(b"S")
                        .ok()
                        .and_then(|o| o.as_name_str().ok())
                        .map(|s| s == "GoTo")
                        .unwrap_or(false);
                    
                    if is_goto {
                        if let Ok(d) = action.get(b"D") {
                            // ★修正点: docを渡す
                            target_page_num = resolve_dest(doc, d, object_id_to_page, named_dests);
                        }
                    }
                }
            }

            if let Some(page_num) = target_page_num {
                results.push((page_num, title));
            } else {
                log.push(format!("Skipped: '{}' (Page not resolved)", title));
            }

            if let Ok(child_ref) = item.get(b"First").and_then(|o| o.as_reference()) {
                collect_bookmarks_recursive(doc, child_ref, object_id_to_page, named_dests, results, log);
            }

            current_id_opt = item.get(b"Next")
                .ok()
                .and_then(|o| o.as_reference().ok());
        } else {
            break;
        }
    }
}

/// Destination解決ヘルパー (Reference解決機能付き)
fn resolve_dest(
    doc: &Document,
    dest_obj: &Object, 
    page_map: &BTreeMap<ObjectId, u32>,
    named_dests: &HashMap<Vec<u8>, Object>
) -> Option<u32> {
    
    // 1. まずReferenceなら実体を取得（これが重要）
    let real_dest = match resolve_object(doc, dest_obj) {
        Ok(o) => o,
        Err(_) => return None,
    };

    // 2. 配列パターン [PageRef, /Fit...]
    if let Ok(arr) = real_dest.as_array() {
        if let Some(Ok(page_ref)) = arr.get(0).map(|o| o.as_reference()) {
            return page_map.get(&page_ref).cloned();
        }
        return None;
    }

    // 3. 名前付き宛先 (Name or String)
    let key = match real_dest {
        Object::String(bytes, _) => Some(bytes.clone()),
        Object::Name(bytes) => Some(bytes.clone()),
        _ => None,
    };

    if let Some(k) = key {
        // マップから検索
        if let Some(target_obj) = named_dests.get(&k) {
            // マップの中身もReferenceの可能性があるので解決
            if let Ok(resolved_target) = resolve_object(doc, target_obj) {
                // 配列ならページ取得
                if let Ok(arr) = resolved_target.as_array() {
                    if let Some(Ok(page_ref)) = arr.get(0).map(|o| o.as_reference()) {
                        return page_map.get(&page_ref).cloned();
                    }
                }
                // 辞書(D)ならさらにその中の /D を見るケースもある
                if let Ok(dict) = resolved_target.as_dict() {
                    if let Ok(inner_d) = dict.get(b"D") {
                        // 再帰的に解決（無限ループ防止のため簡易的に1回だけ）
                         if let Ok(inner_arr) = resolve_object(doc, inner_d).and_then(|o| o.as_array()) {
                             if let Some(Ok(page_ref)) = inner_arr.get(0).map(|o| o.as_reference()) {
                                 return page_map.get(&page_ref).cloned();
                             }
                         }
                    }
                }
            }
        }
    }

    None
}