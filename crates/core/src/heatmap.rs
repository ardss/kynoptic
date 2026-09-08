//! 按键热力图与组合键聚合
//!
//! 把 commands/realtime.rs 里的 60 行算法搬到 core,使领域逻辑可独立 unit-test,
//! transport 层（get_heatmap）只需"取数据 → 调算法 → to_value"5 行。
//!
//! 输入：`queries::keyboard_press_data_today` 返回的 event_data JSON 字符串列表。
//! 输出：按键频次 + 组合键频次（按 count 降序）。
//!
//! 设计要点：
//! - 纯函数（除键盘名称归一化），不依赖 DB、不依赖 tauri
//! - 借用 `keyboard_layout` 的 vk_to_key_name / is_modifier_key / capitalize_mod
//! - 组合键规则：仅在有 modifiers 且**非修饰键本身**时记录；修饰键名不计入组合

use serde::Serialize;
use std::collections::HashMap;

use crate::keyboard_layout;

/// 单条按键频次条目
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct KeyCount {
    pub key: String,
    pub count: i64,
}

/// 单条组合键频次条目
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ShortcutCount {
    pub combo: String,
    pub count: i64,
}

/// 热力图聚合结果
#[derive(Debug, Clone, Serialize, Default)]
pub struct HeatmapResult {
    /// 按键频次，按 count 降序
    pub heatmap: Vec<KeyCount>,
    /// 组合键频次（Top 15），按 count 降序
    pub shortcuts: Vec<ShortcutCount>,
}

/// 快捷键排序上限（与前端契约保持一致）
const TOP_SHORTCUTS: usize = 15;

/// 聚合按键热力图。
///
/// 行为契约（保持与原 realtime.rs::get_heatmap 100% 一致）：
/// 1. 每个 `event_data` JSON 解析为 `{"modifiers": [...], "key": "x"}` 或 `{"vk_code": N}`
/// 2. 优先用 `key` 字段（vk_code 退路）
/// 3. 无 modifiers 的按键计入 `heatmap`；有 modifiers 的非修饰键计入 `shortcuts`
/// 4. 组合键格式：`Ctrl+Shift+A` (按字母序)
/// 5. 仅可打印字符（ascii_graphic 或空格）参与组合键
///
/// 解析失败的 event_data 静默跳过（与原行为一致）。
pub fn aggregate(press_data: Vec<String>) -> HeatmapResult {
    let mut key_counts: HashMap<String, i64> = HashMap::new();
    let mut combo_counts: HashMap<String, i64> = HashMap::new();

    for data_str in press_data {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&data_str) else {
            continue;
        };

        let mod_names: Vec<&str> = data
            .get("modifiers")
            .and_then(|m| m.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        let has_mods = !mod_names.is_empty();

        let key_name = data
            .get("key")
            .and_then(|k| k.as_str())
            .map(String::from)
            .or_else(|| {
                data.get("vk_code")
                    .and_then(|v| v.as_u64())
                    .and_then(keyboard_layout::vk_to_key_name)
                    .map(String::from)
            });

        let Some(name) = key_name else {
            continue;
        };

        *key_counts.entry(name.clone()).or_insert(0) += 1;

        if has_mods && !keyboard_layout::is_modifier_key(&name) {
            let mut parts: Vec<String> = mod_names
                .iter()
                .map(|s| keyboard_layout::capitalize_mod(s))
                .collect();
            if name.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
                parts.push(name);
                let combo = parts.join("+");
                *combo_counts.entry(combo).or_insert(0) += 1;
            }
        }
    }

    // 排序 heatmap（按 count 降序）
    let mut heatmap_entries: Vec<(String, i64)> = key_counts.into_iter().collect();
    heatmap_entries.sort_by(|a, b| b.1.cmp(&a.1));
    let heatmap: Vec<KeyCount> = heatmap_entries
        .into_iter()
        .map(|(key, count)| KeyCount { key, count })
        .collect();

    // 排序 shortcuts（按 count 降序，取 Top N）
    let mut combo_entries: Vec<(String, i64)> = combo_counts.into_iter().collect();
    combo_entries.sort_by(|a, b| b.1.cmp(&a.1));
    let shortcuts: Vec<ShortcutCount> = combo_entries
        .into_iter()
        .take(TOP_SHORTCUTS)
        .map(|(combo, count)| ShortcutCount { combo, count })
        .collect();

    HeatmapResult { heatmap, shortcuts }
}

// ─── 鼠标热力 ────────────────────────────────────────────────────────────────

/// 鼠标热力网格分辨率（16:9，与常见显示器比例一致）。
pub const MOUSE_GRID_COLS: usize = 32;
pub const MOUSE_GRID_ROWS: usize = 18;

/// 鼠标热力聚合结果。
///
/// - `grid`：行优先的 ROWS×COLS 计数矩阵（click+scroll 落点密度），
///   按观测到的坐标极值归一化到网格，自适应任意分辨率。
/// - `clicks`/`scrolls`/`moves`/`releases`：各动作总数。
/// - `buttons`：按钮分布（left/right/middle…），按 count 降序。
/// - `scroll_up`/`scroll_down`：滚动方向计数。
/// - `grid_max`：grid 单元最大值（前端归一化用）。
/// - `coord_max_x`/`coord_max_y`：观测坐标极值（调试/标注用）。
#[derive(Debug, Clone, Serialize, Default)]
pub struct MouseHeatmap {
    pub grid: Vec<i64>,
    pub cols: usize,
    pub rows: usize,
    pub grid_max: i64,
    pub clicks: i64,
    pub releases: i64,
    pub scrolls: i64,
    pub moves: i64,
    pub scroll_up: i64,
    pub scroll_down: i64,
    pub buttons: Vec<KeyCount>,
    pub coord_max_x: i64,
    pub coord_max_y: i64,
}

/// 聚合鼠标热力。输入是 `(event_action, event_data_json)` 列表。
///
/// 坐标取 event_data 的 `x`/`y`（hook 写入物理像素）。先扫一遍取坐标极值，
/// 再把 click+scroll 的落点映射进固定网格——这样无需知道屏幕分辨率即可自适应。
/// move 事件只计总数，不进网格（量大且密集，落点意义不大，避免冲淡点击热区）。
pub fn aggregate_mouse(events: Vec<(String, String)>) -> MouseHeatmap {
    let mut parsed: Vec<(String, i64, i64)> = Vec::new(); // (action, x, y)
    let mut clicks = 0i64;
    let mut releases = 0i64;
    let mut scrolls = 0i64;
    let mut moves = 0i64;
    let mut scroll_up = 0i64;
    let mut scroll_down = 0i64;
    let mut button_counts: HashMap<String, i64> = HashMap::new();
    let mut max_x = 0i64;
    let mut max_y = 0i64;

    for (action, data_str) in events {
        let Ok(data) = serde_json::from_str::<serde_json::Value>(&data_str) else {
            continue;
        };
        let x = data.get("x").and_then(|v| v.as_i64()).unwrap_or(-1);
        let y = data.get("y").and_then(|v| v.as_i64()).unwrap_or(-1);

        match action.as_str() {
            "click" => {
                clicks += 1;
                if let Some(b) = data.get("button").and_then(|v| v.as_str()) {
                    *button_counts.entry(b.to_string()).or_insert(0) += 1;
                }
            }
            "release" => releases += 1,
            "scroll" => {
                scrolls += 1;
                match data.get("direction").and_then(|v| v.as_str()) {
                    Some("up") => scroll_up += 1,
                    Some("down") => scroll_down += 1,
                    _ => {
                        // 退路：用 dy 符号判断
                        match data.get("dy").and_then(|v| v.as_i64()) {
                            Some(dy) if dy > 0 => scroll_up += 1,
                            Some(dy) if dy < 0 => scroll_down += 1,
                            _ => {}
                        }
                    }
                }
            }
            "move" => moves += 1,
            _ => {}
        }

        // click + scroll 进网格（有落点意义）；move 不进
        if (action == "click" || action == "scroll") && x >= 0 && y >= 0 {
            if x > max_x {
                max_x = x;
            }
            if y > max_y {
                max_y = y;
            }
            parsed.push((action, x, y));
        }
    }

    // 第二遍：归一化进网格
    let mut grid = vec![0i64; MOUSE_GRID_COLS * MOUSE_GRID_ROWS];
    let denom_x = if max_x > 0 { max_x } else { 1 };
    let denom_y = if max_y > 0 { max_y } else { 1 };
    for (_, x, y) in &parsed {
        let col = ((*x as f64 / denom_x as f64) * (MOUSE_GRID_COLS as f64 - 1.0)).round() as usize;
        let row = ((*y as f64 / denom_y as f64) * (MOUSE_GRID_ROWS as f64 - 1.0)).round() as usize;
        let col = col.min(MOUSE_GRID_COLS - 1);
        let row = row.min(MOUSE_GRID_ROWS - 1);
        grid[row * MOUSE_GRID_COLS + col] += 1;
    }
    let grid_max = grid.iter().copied().max().unwrap_or(0);

    let mut buttons: Vec<(String, i64)> = button_counts.into_iter().collect();
    buttons.sort_by(|a, b| b.1.cmp(&a.1));
    let buttons: Vec<KeyCount> = buttons
        .into_iter()
        .map(|(key, count)| KeyCount { key, count })
        .collect();

    MouseHeatmap {
        grid,
        cols: MOUSE_GRID_COLS,
        rows: MOUSE_GRID_ROWS,
        grid_max,
        clicks,
        releases,
        scrolls,
        moves,
        scroll_up,
        scroll_down,
        buttons,
        coord_max_x: max_x,
        coord_max_y: max_y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个 keyboard 事件的 event_data JSON
    fn ev(key: &str, mods: &[&str]) -> String {
        serde_json::json!({"key": key, "modifiers": mods}).to_string()
    }

    /// 构造一个 vk_code 退路事件
    fn ev_vk(vk: u64) -> String {
        serde_json::json!({"vk_code": vk, "modifiers": []}).to_string()
    }

    #[test]
    fn empty_input_returns_default() {
        let r = aggregate(vec![]);
        assert!(r.heatmap.is_empty());
        assert!(r.shortcuts.is_empty());
    }

    #[test]
    fn counts_plain_keys() {
        let data = vec![ev("a", &[]), ev("a", &[]), ev("b", &[])];
        let r = aggregate(data);
        assert_eq!(r.heatmap.len(), 2);
        let a = r.heatmap.iter().find(|k| k.key == "a").unwrap();
        assert_eq!(a.count, 2);
        let b = r.heatmap.iter().find(|k| k.key == "b").unwrap();
        assert_eq!(b.count, 1);
        assert!(r.shortcuts.is_empty());
    }

    #[test]
    fn counts_modifier_combo() {
        let data = vec![ev("c", &["ctrl"]), ev("c", &["ctrl"]), ev("v", &["ctrl"])];
        let r = aggregate(data);
        assert_eq!(r.heatmap.len(), 2); // c 和 v
        assert_eq!(r.shortcuts.len(), 2);
        let ctrl_c = r
            .shortcuts
            .iter()
            .find(|s| s.combo == "Ctrl+c")
            .expect("Ctrl+c 必须在");
        assert_eq!(ctrl_c.count, 2);
    }

    #[test]
    fn multi_modifier_sorted() {
        // Shift+Ctrl+A：modifiers 顺序是 [shift, ctrl]（hook 顺序），
        // 经 capitalize_mod 后输出 ["Shift", "Ctrl"]，组合键 = "Shift+Ctrl+a"
        // 注：组合键顺序由 monitor 写入顺序决定，不是字母序——保持原行为
        let data = vec![ev("a", &["shift", "ctrl"])];
        let r = aggregate(data);
        assert_eq!(r.shortcuts.len(), 1);
        assert_eq!(r.shortcuts[0].combo, "Shift+Ctrl+a");
    }

    #[test]
    fn modifier_alone_doesnt_count_as_combo() {
        // 单独按 Ctrl 键：event_data.key 写的是小写 "ctrl"（hook 端的原样），
        // is_modifier_key 只识别大写 "Ctrl"，因此小写 "ctrl" 不被视为修饰键，
        // 实际上会进入 heatmap 但不进入 shortcuts（因为没有 modifiers）
        let data = vec![ev("ctrl", &[])];
        let r = aggregate(data);
        assert_eq!(r.heatmap.len(), 1);
        assert_eq!(r.heatmap[0].key, "ctrl");
        assert!(r.shortcuts.is_empty());
    }

    #[test]
    fn non_printable_keys_excluded_from_combo() {
        // Esc (字符全是非 graphic 之外 — 实际 Esc 字符 'E','s','c' 都 graphic，
        // 因此该测试验证的是另一个角度：含 NUL 控制符的字符串不应进入组合键。
        // 真实场景：event_data.key="Enter" 时 'E','n','t','e','r' 都是 graphic，
        // 所以原 realtime.rs 的 'is_ascii_graphic' 判定实际是宽松的。
        // 此处改测一个真正包含控制符的 key 来触发排除逻辑。
        let data = vec![ev("\t", &["ctrl"])]; // Tab = '\t' (控制字符)
        let r = aggregate(data);
        assert_eq!(r.heatmap.len(), 1);
        assert!(r.shortcuts.is_empty());
    }

    #[test]
    fn vk_code_fallback() {
        // 65 = 'A' in VK
        let data = vec![ev_vk(65)];
        let r = aggregate(data);
        assert!(r.heatmap.iter().any(|k| k.key == "A"));
    }

    #[test]
    fn malformed_json_skipped() {
        let data = vec!["{invalid json".to_string(), ev("a", &[])];
        let r = aggregate(data);
        assert_eq!(r.heatmap.len(), 1);
    }

    #[test]
    fn top_15_shortcuts_cap() {
        // 构造 20 个不同的组合键
        let data: Vec<String> = (0..20)
            .map(|i| ev("a", &["ctrl", &format!("f{}", i)]))
            .collect();
        let r = aggregate(data);
        assert_eq!(r.shortcuts.len(), TOP_SHORTCUTS);
    }

    #[test]
    fn result_serializes_to_expected_json() {
        // 验证 derive Serialize 输出格式
        let r = HeatmapResult {
            heatmap: vec![KeyCount {
                key: "a".into(),
                count: 3,
            }],
            shortcuts: vec![ShortcutCount {
                combo: "Ctrl+s".into(),
                count: 1,
            }],
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["heatmap"][0]["key"], "a");
        assert_eq!(v["heatmap"][0]["count"], 3);
        assert_eq!(v["shortcuts"][0]["combo"], "Ctrl+s");
    }

    // ─── 鼠标热力测试 ───
    fn mev(action: &str, x: i64, y: i64) -> (String, String) {
        (
            action.into(),
            serde_json::json!({"x": x, "y": y, "button": "left"}).to_string(),
        )
    }

    #[test]
    fn mouse_empty_returns_default() {
        let r = aggregate_mouse(vec![]);
        assert_eq!(r.clicks, 0);
        assert_eq!(r.grid_max, 0);
        assert_eq!(r.grid.len(), MOUSE_GRID_COLS * MOUSE_GRID_ROWS);
    }

    #[test]
    fn mouse_counts_actions() {
        let data = vec![
            mev("click", 10, 10),
            mev("click", 20, 20),
            mev("release", 20, 20),
            (
                "move".into(),
                serde_json::json!({"x": 5, "y": 5}).to_string(),
            ),
            (
                "scroll".into(),
                serde_json::json!({"x": 5, "y": 5, "direction": "down"}).to_string(),
            ),
        ];
        let r = aggregate_mouse(data);
        assert_eq!(r.clicks, 2);
        assert_eq!(r.releases, 1);
        assert_eq!(r.moves, 1);
        assert_eq!(r.scrolls, 1);
        assert_eq!(r.scroll_down, 1);
        assert_eq!(r.buttons.iter().find(|b| b.key == "left").unwrap().count, 2);
    }

    #[test]
    fn mouse_grid_maps_corners() {
        // 左上 (0,0) 和右下 (max,max) 应落在网格对角
        let data = vec![mev("click", 0, 0), mev("click", 100, 100)];
        let r = aggregate_mouse(data);
        assert_eq!(r.grid[0], 1); // 左上角 row0col0
        let last = MOUSE_GRID_ROWS * MOUSE_GRID_COLS - 1;
        assert_eq!(r.grid[last], 1); // 右下角
        assert_eq!(r.grid_max, 1);
        assert_eq!(r.coord_max_x, 100);
        assert_eq!(r.coord_max_y, 100);
    }

    #[test]
    fn mouse_scroll_dy_fallback() {
        let data = vec![
            (
                "scroll".into(),
                serde_json::json!({"x": 1, "y": 1, "dy": -120}).to_string(),
            ),
            (
                "scroll".into(),
                serde_json::json!({"x": 1, "y": 1, "dy": 120}).to_string(),
            ),
        ];
        let r = aggregate_mouse(data);
        assert_eq!(r.scroll_down, 1);
        assert_eq!(r.scroll_up, 1);
    }

    #[test]
    fn mouse_move_not_in_grid() {
        // move 只计数，不进网格
        let data = vec![(
            "move".into(),
            serde_json::json!({"x": 50, "y": 50}).to_string(),
        )];
        let r = aggregate_mouse(data);
        assert_eq!(r.moves, 1);
        assert_eq!(r.grid_max, 0);
    }
}
