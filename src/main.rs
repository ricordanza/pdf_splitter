use anyhow::{Context, Result};
use clap::Parser;
use lopdf::{Document, Object};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// PDFを章（ブックマーク）ごとに分割するツール
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 入力PDFファイルのパス
    #[arg(help = "分割したいPDFファイルのパスを指定してください")]
    input_path: PathBuf,
}

/// PDFの文字列（バイト列）をRustのStringに変換するヘルパー関数
/// PDFのテキスト文字列は PDFDocEncoding または UTF-16BE (BOM付き) が使われます。
fn decode_pdf_string(bytes: &[u8]) -> String {
    // UTF-16BE (BOM: FE FF) のチェック
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let u16_vec: Vec<u16> = bytes[2..] // BOMをスキップ
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        
        // UTF-16からStringへ変換
        String::from_utf16(&u16_vec).unwrap_or_else(|_| {
            // 失敗した場合は無理やりUTF-8として表示（デバッグ用）
            String::from_utf8_lossy(bytes).to_string()
        })
    } else {
        // それ以外（PDFDocEncodingやASCII）
        // 厳密にはPDFDocEncodingからUnicodeへのマッピングが必要ですが、
        // 多くの場合、英数字であれば UTF-8 lossy で読めます。
        // 日本語PDFでここに来るケースは稀です（Shift-JIS等の独自拡張を除く）。
        String::from_utf8_lossy(bytes).to_string()
    }
}

/// ファイル名に使えない文字を置換する関数
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            // Windows/Mac/Linuxで禁止・非推奨の文字をアンダースコアに置換
            match c {
                '/' | '\\' | '?' | '%' | '*' | ':' | '|' | '"' | '<' | '>' | '.' => '_',
                // 制御文字も除去
                c if c.is_control() => '_',
                _ => c,
            }
        })
        .collect()
}

fn main() -> Result<()> {
    // 1. 引数を解析する
    let args = Args::parse();
    let input_path = args.input_path;

    let file_stem = input_path
        .file_stem()
        .context("Invalid file name")?
        .to_string_lossy();
    let parent_dir = input_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));

    println!("Loading PDF: {:?}", input_path);

    let mut doc = Document::load(&input_path)
        .with_context(|| format!("Failed to load PDF: {:?}", input_path))?;

    // 2. ページIDとページ番号の対応表を作る
    let page_numbers = doc.get_pages();
    let object_id_to_page: BTreeMap<_, _> = page_numbers
        .iter()
        .map(|(num, id)| (*id, *num))
        .collect();

    // 3. 目次（アウトライン）から章の開始ページを取得する
    let mut chapter_starts = Vec::new();

    if let Ok(catalog_ref) = doc.trailer.get(b"Root").and_then(|o| o.as_reference()) {
        if let Ok(catalog) = doc.get_object(catalog_ref).and_then(|o| o.as_dict()) {
            
            if let Ok(outlines_ref) = catalog.get(b"Outlines").and_then(|o| o.as_reference()) {
                if let Ok(outlines) = doc.get_object(outlines_ref).and_then(|o| o.as_dict()) {
                    
                    let mut current_ref = outlines.get(b"First")
                        .ok()
                        .and_then(|o| o.as_reference().ok());

                    while let Some(id) = current_ref {
                        if let Ok(item) = doc.get_object(id).and_then(|o| o.as_dict()) {
                            
                            let mut target_page_num = None;

                            // Dest (宛先) を確認
                            if let Ok(dest) = item.get(b"Dest") {
                                if let Ok(arr) = dest.as_array() {
                                    if let Some(Ok(page_ref)) = arr.get(0).map(|o| o.as_reference()) {
                                        target_page_num = object_id_to_page.get(&page_ref).cloned();
                                    }
                                }
                            } 

                            if let Some(page_num) = target_page_num {
                                // ★ここで文字コード変換を行う
                                let title = item.get(b"Title")
                                    .ok()
                                    .and_then(|o| o.as_str().ok())
                                    .map(|bytes| decode_pdf_string(bytes)) // 修正箇所
                                    .unwrap_or_else(|| "Chapter".to_string());
                                
                                chapter_starts.push((page_num, title));
                            }

                            current_ref = item.get(b"Next")
                                .ok()
                                .and_then(|o| o.as_reference().ok());
                        } else {
                            break;
                        }
                    }
                }
            }
        }
    }

    if chapter_starts.is_empty() {
        println!("警告: 目次が見つかりませんでした。全ページを1つのファイルとして出力します。");
        chapter_starts.push((1, "FullDocument".to_string()));
    }

    chapter_starts.sort_by_key(|k| k.0);
    chapter_starts.dedup_by_key(|k| k.0);

    let total_pages = page_numbers.len() as u32;
    println!("Found {} chapters/sections.", chapter_starts.len());

    // 4. 分割して保存
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

        if start_page > &end_page { 
            continue; 
        }

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

        // ★日本語ファイル名を維持しつつ、禁止文字だけ置換
        let safe_title = sanitize_filename(title);
        
        let out_filename = format!("{}_chapter_{}_{}.pdf", file_stem, i + 1, safe_title);
        let out_path = parent_dir.join(out_filename);

        split_doc.save(&out_path)?;
    }
    
    println!("Done!");
    Ok(())
}