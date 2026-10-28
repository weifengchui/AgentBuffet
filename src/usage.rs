//! M3 用量记账（设计文档 §8）：JSONL 追加落盘 + 内存 counters + prices 费用估算。
//! 硬约束：prices 未命中该 model 时 cost_est 记 null，**不得按 0 计**；
//! 落盘失败绝不影响转发；启动时顺序读当日 JSONL 一次性重建 counters。

use crate::failover::now_epoch_secs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 单模型单价（每 1M tokens）。
#[derive(Debug, Clone, Deserialize)]
pub struct PriceEntry {
    pub input: f64,
    pub output: f64,
}

pub type Prices = HashMap<String, PriceEntry>;

/// 读取 prices.yaml（model → {input, output}）。
/// 失败仅告警不拒启：返回空表 + 告警文案，此后 cost_est 一律 null。
pub fn load_prices(path: &Path) -> (Prices, Option<String>) {
    // 历史兼容：默认路径迁移到 ~/.agentbuffet 后，旧版放在程序目录的 ./prices.yaml 仍兜底读取
    let path = if !path.exists() && path == crate::config::default_prices_path() {
        std::path::Path::new("./prices.yaml")
    } else {
        path
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            return (
                Prices::new(),
                Some(format!(
                    "读取单价文件 {} 失败: {e}；cost_est 将记 null",
                    path.display()
                )),
            )
        }
    };
    match serde_yaml::from_str::<Prices>(&text) {
        Ok(p) => (p, None),
        Err(e) => (
            Prices::new(),
            Some(format!(
                "解析单价文件 {} 失败: {e}；cost_est 将记 null",
                path.display()
            )),
        ),
    }
}

/// 单条用量事件，serde 序列化即 §8 JSONL 格式。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    pub agent: String,
    pub group: String,
    pub provider: String,
    pub model: String,
    pub status: u16,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_est: Option<f64>,
    pub duration_ms: u64,
    pub retries: u32,
    pub fail_chain: Vec<String>,
}

/// (agent, group, provider) 维度的当日聚合，全部整数加法（费用为 f64 累加）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counters {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_est: f64,
    pub failures: u64,
}

impl Counters {
    fn apply(&mut self, ev: &Event) {
        self.requests += 1;
        self.prompt_tokens += ev.prompt_tokens;
        self.completion_tokens += ev.completion_tokens;
        if let Some(c) = ev.cost_est {
            self.cost_est += c;
        }
        if ev.status >= 400 {
            self.failures += 1;
        }
    }
}

/// 用量记录器：JSONL 落盘 + 内存 counters + 单价表。
pub struct Usage {
    logs_dir: PathBuf,
    prices: Mutex<Prices>,
    counters: Mutex<HashMap<(String, String, String), Counters>>,
}

impl Usage {
    /// 建目录、读单价、顺序读当日 JSONL 重建 counters（坏行跳过）。
    pub fn new(logs_dir: PathBuf, prices_path: &Path) -> Self {
        let _ = std::fs::create_dir_all(&logs_dir);
        let (prices, warn) = load_prices(prices_path);
        if let Some(w) = warn {
            eprintln!("AgentBuffet: [warn] {w}");
        }
        let u = Self {
            logs_dir,
            prices: Mutex::new(prices),
            counters: Mutex::new(HashMap::new()),
        };
        u.rebuild_today();
        u
    }

    /// 当日（UTC）JSONL 路径。
    pub fn day_file(&self, ts: u64) -> PathBuf {
        self.logs_dir.join(format!("usage-{}.jsonl", utc_date(ts)))
    }

    fn rebuild_today(&self) {
        let path = self.day_file(now_epoch_secs());
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let mut g = self.counters.lock().unwrap();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<Event>(line) {
                g.entry((ev.agent.clone(), ev.group.clone(), ev.provider.clone()))
                    .or_default()
                    .apply(&ev);
            }
        }
    }

    /// 追加一行 JSONL 并整数累加 counters；落盘失败静默（不影响转发）。
    pub fn record(&self, ev: &Event) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.day_file(ev.ts))
        {
            if let Ok(line) = serde_json::to_string(ev) {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
        let mut g = self.counters.lock().unwrap();
        g.entry((ev.agent.clone(), ev.group.clone(), ev.provider.clone()))
            .or_default()
            .apply(ev);
    }

    pub fn set_prices(&self, p: Prices) {
        *self.prices.lock().unwrap() = p;
    }

    /// 费用估算：未命中单价返回 None（JSONL 里即 null）。
    pub fn cost(&self, model: &str, prompt: u64, completion: u64) -> Option<f64> {
        let g = self.prices.lock().unwrap();
        let e = g.get(model)?;
        Some(prompt as f64 / 1e6 * e.input + completion as f64 / 1e6 * e.output)
    }

    /// 当前 counters 快照（UI/测试读取，无文件 IO）。
    pub fn counters(&self) -> HashMap<(String, String, String), Counters> {
        self.counters.lock().unwrap().clone()
    }

    /// 最近 n 条流水（Overview 视图）：只读当日 JSONL 末尾至多 1 MiB，丢半行、坏行跳过。
    pub fn recent(&self, n: usize) -> Vec<Event> {
        use std::io::{Read, Seek, SeekFrom};
        const CAP: u64 = 1 << 20;
        let path = self.day_file(now_epoch_secs());
        let Ok(mut f) = std::fs::File::open(&path) else {
            return Vec::new();
        };
        let size = match f.seek(SeekFrom::End(0)) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let start = size.saturating_sub(CAP);
        if f.seek(SeekFrom::Start(start)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        // 从中间截断读取时，首行可能是半行，丢弃之
        if start > 0 {
            if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                buf.drain(0..=pos);
            }
        }
        let text = String::from_utf8_lossy(&buf);
        let mut out: Vec<Event> = text
            .lines()
            .rev()
            .filter_map(|l| serde_json::from_str::<Event>(l.trim()).ok())
            .take(n)
            .collect();
        out.reverse();
        out
    }
}

/// epoch 秒 → "YYYY-MM-DD"（UTC）。days→civil 自算（Howard Hinnant 算法），不引 chrono。
pub fn utc_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_date_known_values() {
        assert_eq!(utc_date(0), "1970-01-01");
        // 设计文档 §8 示例 ts=1729660800 → 2024-10-23 05:20 UTC
        assert_eq!(utc_date(1_729_660_800), "2024-10-23");
        assert_eq!(utc_date(1_735_689_599), "2024-12-31");
        assert_eq!(utc_date(1_735_689_600), "2025-01-01");
        // 闰年 2024-02-29
        assert_eq!(utc_date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn cost_hit_and_miss() {
        let dir = std::env::temp_dir().join(format!("buffet-usage-cost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let prices_path = dir.join("prices.yaml");
        std::fs::write(&prices_path, "m1:\n  input: 2.0\n  output: 8.0\n").unwrap();
        let u = Usage::new(dir.join("logs"), &prices_path);
        // 每 1M tokens：1M*2 + 0.5M*8 = 6.0
        let c = u.cost("m1", 1_000_000, 500_000).unwrap();
        assert!((c - 6.0).abs() < 1e-9);
        assert!(u.cost("unknown", 100, 100).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_then_rebuild_counters() {
        let dir = std::env::temp_dir().join(format!("buffet-usage-rebuild-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let logs = dir.join("logs");
        let prices = dir.join("no-such-prices.yaml");
        let now = now_epoch_secs();
        let u = Usage::new(logs.clone(), &prices);
        let mk = |status: u16| Event {
            ts: now,
            agent: "hermes".into(),
            group: "g".into(),
            provider: "p1".into(),
            model: "m".into(),
            status,
            prompt_tokens: 10,
            completion_tokens: 5,
            cost_est: None,
            duration_ms: 100,
            retries: 0,
            fail_chain: vec!["p1:ok".into()],
        };
        u.record(&mk(200));
        u.record(&mk(500));
        // 坏行必须被跳过
        std::fs::OpenOptions::new()
            .append(true)
            .open(u.day_file(now))
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let u2 = Usage::new(logs, &prices);
        let c = u2.counters();
        let one = &c[&("hermes".into(), "g".into(), "p1".into())];
        assert_eq!(one.requests, 2);
        assert_eq!(one.prompt_tokens, 20);
        assert_eq!(one.completion_tokens, 10);
        assert_eq!(one.failures, 1);
        assert_eq!(one.cost_est, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
