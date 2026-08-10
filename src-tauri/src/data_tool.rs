use std::{fs, path::Path};

use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    ensure_writable_directory, path_guard, string_argument, truncate_output, workspace_root,
};

pub(crate) fn parse_csv_line(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                values.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    values.push(current.trim().to_string());
    values
}

fn value_to_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => value.to_string(),
    }
}

fn io_compatible_path(path: std::path::PathBuf) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest);
        }
    }
    path
}

pub(crate) fn table_from_contents(
    path: &str,
    contents: &str,
) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    if path.to_lowercase().ends_with(".json") {
        let value: Value = serde_json::from_str(contents)
            .map_err(|error| format!("Unable to parse JSON data: {error}"))?;
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())
            .cloned()
            .unwrap_or_default();
        let mut headers = Vec::new();
        for row in &rows {
            if let Some(object) = row.as_object() {
                for key in object.keys() {
                    if !headers.contains(key) {
                        headers.push(key.clone());
                    }
                }
            }
        }
        if headers.is_empty() && !rows.is_empty() {
            headers.push("value".to_string());
        }
        let cells = rows
            .iter()
            .map(|row| {
                if let Some(object) = row.as_object() {
                    headers
                        .iter()
                        .map(|header| object.get(header).map(value_to_cell).unwrap_or_default())
                        .collect::<Vec<_>>()
                } else {
                    vec![value_to_cell(row)]
                }
            })
            .collect();
        return Ok((headers, cells));
    }
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());
    let headers = parse_csv_line(lines.next().unwrap_or_default());
    if headers.is_empty() || headers.iter().all(String::is_empty) {
        return Err("CSV data has no header row.".to_string());
    }
    Ok((headers, lines.map(parse_csv_line).collect()))
}

pub(crate) async fn load_table(path: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("Unable to read data file {path}: {error}"))?;
    table_from_contents(path, &contents)
}

pub(crate) async fn run_data_analysis_tool(arguments: &Value) -> Result<String, String> {
    let path = string_argument(arguments, "path")
        .or_else(|| string_argument(arguments, "json_path"))
        .ok_or_else(|| "rust_data_analysis requires path".to_string())?;
    let (headers, rows) = load_table(&path).await?;
    let mut missing = vec![0usize; headers.len()];
    let mut numeric_sum = vec![0.0f64; headers.len()];
    let mut numeric_count = vec![0usize; headers.len()];
    let sample_limit = arguments
        .get("sample_rows")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let mut sample = Vec::new();
    for values in &rows {
        if sample.len() < sample_limit {
            sample.push(values.clone());
        }
        for index in 0..headers.len() {
            let value = values.get(index).map(String::as_str).unwrap_or_default();
            if value.is_empty() {
                missing[index] += 1;
            } else if let Ok(number) = value.parse::<f64>() {
                numeric_sum[index] += number;
                numeric_count[index] += 1;
            }
        }
    }
    let summaries = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            json!({
                "name": header,
                "missing": missing[index],
                "numeric_count": numeric_count[index],
                "mean": (numeric_count[index] > 0).then_some(numeric_sum[index] / numeric_count[index] as f64)
            })
        })
        .collect::<Vec<_>>();
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "format": "csv",
            "rows": rows.len(),
            "columns": headers,
            "summaries": summaries,
            "sample": sample
        }))
        .unwrap_or_default(),
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn chart_values(headers: &[String], rows: &[Vec<String>]) -> (Vec<String>, Vec<f64>) {
    let numeric_index = headers
        .iter()
        .enumerate()
        .find(|(index, _)| {
            rows.iter().any(|row| {
                row.get(*index)
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some()
            })
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    let labels = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            row.first()
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| (index + 1).to_string())
        })
        .collect::<Vec<_>>();
    let values = rows
        .iter()
        .map(|row| {
            row.get(numeric_index)
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    (labels, values)
}

fn render_svg_chart(title: &str, labels: &[String], values: &[f64]) -> String {
    let width = 900.0;
    let height = 500.0;
    let max = values.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let bar_width = if values.is_empty() {
        0.0
    } else {
        760.0 / values.len() as f64
    };
    let bars = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let bar_height = (value.max(0.0) / max) * 360.0;
            let x = 90.0 + index as f64 * bar_width + bar_width * 0.12;
            let y = 420.0 - bar_height;
            format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{bar_height:.1}\" fill=\"#c45132\"/><text x=\"{:.1}\" y=\"445\" font-size=\"11\" text-anchor=\"middle\">{}</text>",
                (bar_width * 0.76).max(3.0),
                x + (bar_width * 0.76).max(3.0) / 2.0,
                escape_html(labels.get(index).map(String::as_str).unwrap_or_default())
            )
        })
        .collect::<String>();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\"><rect width=\"100%\" height=\"100%\" fill=\"#fbfaf7\"/><text x=\"40\" y=\"42\" font-family=\"sans-serif\" font-size=\"22\" fill=\"#262522\">{}</text><line x1=\"90\" y1=\"60\" x2=\"90\" y2=\"420\" stroke=\"#77736b\"/><line x1=\"90\" y1=\"420\" x2=\"850\" y2=\"420\" stroke=\"#77736b\"/>{bars}</svg>",
        escape_html(title)
    )
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut checksum = Vec::with_capacity(4 + data.len());
    checksum.extend_from_slice(kind);
    checksum.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&checksum).to_be_bytes());
    chunk
}

pub(crate) fn write_png_chart(path: &Path, values: &[f64]) -> Result<(), String> {
    let width = 900usize;
    let height = 500usize;
    let stride = width * 3;
    let mut pixels = vec![255u8; stride * height];
    let set_pixel = |pixels: &mut [u8], x: usize, y: usize, color: [u8; 3]| {
        if x < width && y < height {
            let index = y * stride + x * 3;
            pixels[index..index + 3].copy_from_slice(&color);
        }
    };
    for x in 90..850 {
        set_pixel(&mut pixels, x, 420, [119, 115, 107]);
    }
    for y in 60..421 {
        set_pixel(&mut pixels, 90, y, [119, 115, 107]);
    }
    let max = values.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let bar_width = if values.is_empty() {
        0
    } else {
        760 / values.len()
    };
    for (index, value) in values.iter().enumerate() {
        let bar_height = ((value.max(0.0) / max) * 360.0) as usize;
        let start_x = 90 + index * bar_width + bar_width / 8;
        let end_x = (start_x + bar_width * 3 / 4).min(width);
        let start_y = 420usize.saturating_sub(bar_height);
        for y in start_y..420 {
            for x in start_x..end_x {
                set_pixel(&mut pixels, x, y, [196, 81, 50]);
            }
        }
    }
    let mut raw = Vec::with_capacity((stride + 1) * height);
    for row in pixels.chunks(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut compressed = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < raw.len() {
        let length = (raw.len() - offset).min(65_535);
        let final_block = offset + length == raw.len();
        compressed.push(u8::from(final_block));
        compressed.extend_from_slice(&(length as u16).to_le_bytes());
        compressed.extend_from_slice(&(!(length as u16)).to_le_bytes());
        compressed.extend_from_slice(&raw[offset..offset + length]);
        offset += length;
    }
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in &raw {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    compressed.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &header));
    png.extend_from_slice(&png_chunk(b"IDAT", &compressed));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    fs::write(path, png).map_err(|error| format!("Unable to write chart PNG: {error}"))
}

pub(crate) fn run_visualization_preparation(
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let kind = string_argument(arguments, "kind").unwrap_or_else(|| "bar".to_string());
    let title =
        string_argument(arguments, "title").unwrap_or_else(|| "RustPilot chart".to_string());
    let labels = arguments
        .get("labels")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let values = arguments
        .get("values")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let specification = json!({
        "type": kind,
        "title": title,
        "labels": labels,
        "values": values
    });
    if let Some(path) = string_argument(arguments, "output_path") {
        let path =
            path_guard::resolve_mutation_path(&workspace_root(), &path, external_path_approved)?
                .canonical;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&specification).unwrap_or_default(),
        )
        .map_err(|error| format!("Unable to write visualization specification: {error}"))?;
    }
    Ok(serde_json::to_string_pretty(&json!({
        "specification": specification,
        "renderer": "rustpilot_svg_png_html"
    }))
    .unwrap_or_default())
}

pub(crate) async fn run_data_visualization_tool(arguments: &Value) -> Result<String, String> {
    let input_path = string_argument(arguments, "path")
        .or_else(|| string_argument(arguments, "json_path"))
        .ok_or_else(|| "rust_data_visualization requires path or json_path".to_string())?;
    let output_type = string_argument(arguments, "output_type")
        .unwrap_or_else(|| "html".to_string())
        .to_lowercase();
    let tool_type = string_argument(arguments, "tool_type")
        .unwrap_or_else(|| "visualization".to_string())
        .to_lowercase();
    let mut sources = Vec::new();
    let descriptor = if input_path.to_lowercase().ends_with(".json") {
        tokio::fs::read_to_string(&input_path)
            .await
            .ok()
            .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
    } else {
        None
    };
    if let Some(Value::Array(items)) = descriptor {
        for item in items {
            let path = item
                .get("csvFilePath")
                .or_else(|| item.get("path"))
                .or_else(|| item.get("file"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
            if let Some(path) = path {
                sources.push((
                    path,
                    item.get("chartTitle")
                        .or_else(|| item.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("RustPilot data chart")
                        .to_string(),
                ));
            }
        }
    }
    if sources.is_empty() {
        sources.push((
            input_path,
            string_argument(arguments, "title")
                .unwrap_or_else(|| "RustPilot data report".to_string()),
        ));
    }
    let workspace = workspace_root();
    let output_dir = path_guard::resolve_scoped_path(&workspace, ".rustpilot/visualization")?;
    let output_dir = io_compatible_path(ensure_writable_directory(output_dir, "visualization")?);
    let mut results = Vec::new();
    for (source_path, title) in sources.into_iter().take(16) {
        let source_path = if Path::new(&source_path).is_absolute() {
            source_path
        } else {
            workspace_root().join(source_path).display().to_string()
        };
        let (headers, rows) = load_table(&source_path).await?;
        let (labels, values) = chart_values(&headers, &rows);
        let slug = title
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let stem = format!("{}_{}", slug.trim_matches('_'), Uuid::new_v4().simple());
        let chart_path = if output_type == "png" {
            output_dir.join(format!("{stem}.png"))
        } else {
            output_dir.join(format!("{stem}.html"))
        };
        if output_type == "png" {
            write_png_chart(&chart_path, &values)?;
        } else {
            let svg = render_svg_chart(&title, &labels, &values);
            let rows_html = rows
                .iter()
                .take(100)
                .map(|row| {
                    format!(
                        "<tr>{}</tr>",
                        row.iter()
                            .map(|cell| format!("<td>{}</td>", escape_html(cell)))
                            .collect::<String>()
                    )
                })
                .collect::<String>();
            let html = format!("<!doctype html><meta charset=\"utf-8\"><title>{}</title><style>body{{font-family:system-ui,sans-serif;background:#fbfaf7;color:#262522;margin:32px}}table{{border-collapse:collapse;margin-top:24px}}td,th{{border:1px solid #d5d1c8;padding:6px 9px;text-align:left}}</style><h1>{}</h1>{}<table><thead><tr>{}</tr></thead><tbody>{}</tbody></table>", escape_html(&title), escape_html(&title), svg, headers.iter().map(|header| format!("<th>{}</th>", escape_html(header))).collect::<String>(), rows_html);
            fs::write(&chart_path, html)
                .map_err(|error| format!("Unable to write chart HTML {}: {error}", chart_path.display()))?;
        }
        let insight_path = if tool_type == "insight" {
            let path = output_dir.join(format!("{stem}.md"));
            let numeric = values
                .iter()
                .filter(|value| value.is_finite())
                .copied()
                .collect::<Vec<_>>();
            let average = if numeric.is_empty() {
                0.0
            } else {
                numeric.iter().sum::<f64>() / numeric.len() as f64
            };
            fs::write(
                &path,
                format!(
                    "# {}\n\n- Rows: {}\n- Numeric points: {}\n- Mean: {:.3}\n",
                    title,
                    rows.len(),
                    numeric.len(),
                    average
                ),
            )
            .map_err(|error| format!("Unable to write chart insights {}: {error}", path.display()))?;
            Some(path.display().to_string())
        } else {
            None
        };
        results.push(json!({"title": title, "chart_path": chart_path, "output_type": output_type.clone(), "insight_path": insight_path, "rows": rows.len()}));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "status": "success",
            "observation": "Chart Generated Successful!",
            "results": results
        }))
        .unwrap_or_default(),
    ))
}
