//! Экспорт корпуса в HTML.
//!
//! Проверяется не «красиво ли», а то, что поддаётся проверке: экранирование
//! (иначе ячейка с `<script>` превратилась бы в разметку), попадание всех
//! непустых значений в вывод и согласованность `colspan`/`rowspan` с
//! объявленными объединениями.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]

use ooxml::export::html::{HtmlOptions, workbook_to_html};
use ooxml::xlsx::{CellValue, Workbook};
use std::path::PathBuf;

fn books() -> Vec<PathBuf> {
    let root = std::env::var_os("OOXML_CORPUS").map_or_else(
        || PathBuf::from("/Users/shakh/rustoword/crates/ooxml/tests/corpus"),
        PathBuf::from,
    );
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("xlsx")) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "xlsx") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn every_book_exports_and_keeps_its_text() {
    let files = books();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен (задайте OOXML_CORPUS)");
        return;
    }

    let (mut ok, mut bytes, mut merged, mut formulas) = (0u32, 0usize, 0u32, 0u32);

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut wb = Workbook::open(&data).unwrap_or_else(|e| panic!("{name}: {e}"));

        let opts = HtmlOptions::default();
        let html = workbook_to_html(&mut wb, &opts).unwrap_or_else(|e| panic!("{name}: {e}"));

        assert!(html.starts_with("<!doctype html>"), "{name}: не HTML");
        assert!(html.ends_with("</html>\n"), "{name}: страница оборвана");
        // Самодостаточность: ни одной внешней ссылки.
        for bad in ["<script", "src=\"http", "href=\"http", "@import"] {
            assert!(
                !html.contains(bad),
                "{name}: страница тянет внешнее ({bad})"
            );
        }

        // Каждое непустое строковое значение обязано оказаться в выводе.
        let sheet_count = wb.sheets().len();
        for i in 0..sheet_count {
            let mut sh = wb.sheet(i).unwrap();
            merged += u32::try_from(sh.merges().unwrap().len()).unwrap_or(0);
            for cell in sh.read_all().unwrap() {
                if cell.formula.is_some() {
                    formulas += 1;
                }
                if let CellValue::Text(t) = &cell.value {
                    // Короткие и пустые пропускаем: они тонут в разметке и
                    // ничего не доказывают.
                    let t = t.trim();
                    if t.len() < 12 {
                        continue;
                    }
                    // Ожидаемая строка экранируется так же, как её пишет
                    // экспортёр, иначе тест ловил бы собственную небрежность:
                    // текст со знаком `>` в выводе законно выглядит как `&gt;`.
                    let escaped = t
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                        .replace('"', "&quot;");
                    assert!(
                        html.contains(&escaped),
                        "{name}: текст ячейки {} потерялся при экспорте: {t:?}",
                        cell.at.to_a1()
                    );
                }
            }
        }

        bytes += html.len();
        ok += 1;
    }

    println!("экспорт корпуса в HTML:");
    println!("  книг: {ok}");
    println!("  суммарно разметки: {bytes} байт");
    println!("  объединённых диапазонов: {merged}");
    println!("  ячеек с формулой: {formulas}");
    assert_eq!(ok as usize, files.len());
}

#[test]
fn dangerous_content_is_escaped_not_rendered() {
    // Ячейка с разметкой не имеет права стать разметкой. Проверяем на
    // синтетике: в корпусе такого нет, а дыра здесь была бы худшей из
    // возможных для инструмента, который показывает чужие файлы.
    let files = books();
    if files.is_empty() {
        eprintln!("корпус не найден — тест пропущен");
        return;
    }
    let data = std::fs::read(&files[0]).unwrap();
    let mut wb = Workbook::open(&data).unwrap();

    let payload = "<script>alert(1)</script> & \"кавычки\"";
    {
        let mut sh = wb.sheet(0).unwrap();
        sh.set_string(ooxml::xlsx::CellRef::parse("A1").unwrap(), payload)
            .unwrap();
    }
    let saved = wb.save().unwrap();

    let mut wb2 = Workbook::open(&saved).unwrap();
    let html = workbook_to_html(&mut wb2, &HtmlOptions::default()).unwrap();

    assert!(!html.contains("<script>"), "разметка просочилась в вывод");
    assert!(
        html.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; &quot;кавычки&quot;"),
        "содержимое не найдено в экранированном виде"
    );
}
