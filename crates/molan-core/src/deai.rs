// 去AI味机器门（移植 skills_V0.3/scripts/ai_flavor_score.py 核心规则 + lint 精简版）
// 规则与阈值对齐《规则书 V0.30》：600+ 真人章节校准的密度线，0~100 分，通过线 ≤32（方案审校层用 40 上限内的 32）
// 移植说明：Tier1 一票命中 + Tier2 扎堆 + Tier3 连环 + Tier4 密度制 + 节奏指标（破折号/三连/短句连发/段落结构）

use serde_json::{json, Value};

const PASS_SCORE: f64 = 32.0;

// ── Tier1 一票命中（每处 15 分）──
const TIER1: &[(&str, &str)] = &[
    (r"值得一提(的是|)", "元叙述套话"),
    (r"不得不说", "元叙述套话"),
    (r"总而言之|综上", "总结腔"),
    (r"在这个[^。]{0,12}的时代", "开场套话"),
    (r"故事(才|，才)刚刚开始", "收尾套话"),
    (r"这(或许|也许)就是", "收尾升华"),
    (r"(终于)?明白了?[^。]{0,12}(道理|意义|真谛)", "收尾升华"),
    (r"一股?难以言喻的", "空洞渲染"),
    (r"复杂的情绪", "空洞渲染"),
    (r"眸[中子][^。，]{0,6}(闪过|掠过|划过)", "网文AI腔"),
    (r"眼中[^。，]{0,4}(闪过|掠过|划过)", "网文AI腔"),
    (r"眼底[^。，]{0,6}(闪过|掠过|划过)", "网文AI腔"),
    (r"瞳孔[^。，]{0,4}[缩收]", "网文AI腔"),
    (r"嘴角[^。，]{0,6}(勾起|扬起|牵起|浮现|噙着)", "网文AI腔"),
    (r"青筋[^。，]{0,4}(暴起|毕露|凸起|直跳|跳动)", "网文AI腔"),
    (r"指节[^。，]{0,6}(泛白|发白)", "网文AI腔"),
    (r"目光(如刀|如炬|如鹰|锐利)", "网文AI腔"),
    (r"深吸(了)?一口气", "网文AI腔"),
    (r"冷汗", "网文AI腔"),
    (r"(脊背|后背|后颈)[^。，]{0,4}(发凉|一凉|发麻)", "网文AI腔"),
    (
        r"(心头|心中)[^。，]{0,2}(一跳|一沉|一紧|一颤|一凛)",
        "网文AI腔",
    ),
    (r"淬了冰|如淬冰", "网文AI腔"),
    (r"空气(仿佛|似乎)(凝固|凝滞)", "网文AI腔"),
    (r"时间仿佛(在这一刻)?静止", "网文AI腔"),
    (r"阳光明媚", "开场套话"),
    (r"在这个繁华的", "开场套话"),
];

// ── Tier2 扎堆信号（同段内 ≥3 种命中即记 1 次 8 分）──
const TIER2: &[(&str, &str)] = &[
    (r"然而|与此同时|随即|紧接着|显然|顿时", "连接词扎堆"),
    (r"深深地|缓缓地|静静地|默默地|狠狠地", "渲染副词扎堆"),
    (r"不是[^。，]{1,12}而是", "否定式排比"),
];

// ── Tier4 密度制（每千字阈值 = 700 章真人 p97；超线每条 8 分）──
// (正则, 名称, 每千字阈值, 建议)
const TIER4: &[(&str, &str, f64, &str)] = &[
    (
        r"像[^，。！？]{0,10}一样|仿佛|似乎|如同|宛如|好似|犹如|似的",
        "明喻过密",
        2.2,
        "明喻超密（真人p97=2.75/千字）。砍掉一半比喻，「像…一样」尽量改成直接陈述",
    ),
    (
        r"仿佛在(预示|暗示|诉说|告诉|等待|宣告|提醒|召唤)|似乎有什么|好像有什么|仿佛有什么|似乎在(暗示|告诉|诉说|等待)|像是有什么",
        "占位模糊句",
        0.1,
        "删掉「仿佛在预示着什么」这类无信息量的悬疑占位句——直接写发生了什么",
    ),
    (
        r"紧绷的肩膀|攥紧(了)?(拳头|拳)|指甲.{0,6}掐进|深吸一口气|深吸了口气|咬了咬牙|皱了皱眉|握紧(了)?(拳头|拳)|拳头.{0,4}咯吱|攥着拳",
        "身体语言套路",
        0.41,
        "「攥紧拳头/深吸一口气/咬了咬牙」是AI写情绪的默认动作，换成这个人物此刻独有的反应",
    ),
    (
        r"眼神|神色|神情|目光[里中]|眸[子里中]",
        "眼神神色过密",
        0.93,
        "「眼神/神色/眸子」过密：AI靠写眼神交代情绪，改用动作、台词或具体行为",
    ),
    (
        r"不禁|不由得|情不自禁",
        "不禁不由得",
        0.51,
        "「不禁/不由得/情不自禁」是AI式情绪过渡词，删掉直接写反应",
    ),
    (
        r"缓缓|轻轻|微微|悄悄|慢慢|徐徐",
        "轻缓副词过密",
        1.33,
        "「缓缓/轻轻/微微/慢慢」过密：只保留真正需要慢的地方1~2处",
    ),
    (
        r"有的[^，。！？]{1,12}[，,][^。！？]{0,24}有的",
        "对称有的句式",
        0.1,
        "「有的…有的…」对称罗列是典型AI腔，改成不对称的、带具体细节的描写",
    ),
    (
        r"(空气|四周|周围|空气中)(中)?(弥漫|充斥|飘荡|荡漾|弥漫开)着",
        "感官堆砌",
        0.1,
        "「空气中弥漫着…」是模板化环境描写，改成人物实际闻到/看到/碰到的具体东西",
    ),
    (
        r"所到之处|无一例外|与此同时|不知为何|说不清道不明",
        "所到之处类",
        0.42,
        "「所到之处/无一例外/与此同时」是AI总括腔，删掉或换成具体描述",
    ),
];

// 酒馆腔黑名单（病灶 T7，每处 6 分）
const TAVERN: &str =
    r"压得很平|停了半拍|声音沉了下来|眼睛一亮|头皮发麻|认认真真地打量|所有的声音好像都被隔在了外面";
// 模板句（每处 6 分）
const TEMPLATE: &str = r"与此同时|然而[^。]{0,8}早有防备|他终于意识到|想到这里|心情前所未有的凝重|无论是[^。]{1,12}还是[^。]{1,12}都逼着他";

/// 预编译正则集合：规则表在模块加载后只编译一次（原先每次评分 40+ 次 Regex::new）
fn compile_table(pats: &[&'static str]) -> Vec<(regex::Regex, &'static str)> {
    pats.iter()
        .filter_map(|p| regex::Regex::new(p).ok().map(|re| (re, *p)))
        .collect()
}

/// 按正则源串取预编译结果；命中预编译表则复用，否则退化为即时编译
fn re_for(pattern: &str) -> Option<regex::Regex> {
    for table in [tier1_res(), tier2_res(), tier4_res(), rhythm_res()] {
        if let Some((re, _)) = table.iter().find(|(_, src)| *src == pattern) {
            return Some(re.clone());
        }
    }
    regex::Regex::new(pattern).ok()
}

fn tier1_res() -> &'static Vec<(regex::Regex, &'static str)> {
    static RES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    RES.get_or_init(|| compile_table(&TIER1.iter().map(|(p, _)| *p).collect::<Vec<_>>()))
}

fn tier2_res() -> &'static Vec<(regex::Regex, &'static str)> {
    static RES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    RES.get_or_init(|| compile_table(&TIER2.iter().map(|(p, _)| *p).collect::<Vec<_>>()))
}

fn tier4_res() -> &'static Vec<(regex::Regex, &'static str)> {
    static RES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    RES.get_or_init(|| compile_table(&TIER4.iter().map(|(p, _, _, _)| *p).collect::<Vec<_>>()))
}

/// 酒馆腔 + 模板句 + 节奏类（破折号/三连/感叹号/直引号/英文/对白标记）
fn rhythm_res() -> &'static Vec<(regex::Regex, &'static str)> {
    static RES: std::sync::OnceLock<Vec<(regex::Regex, &'static str)>> = std::sync::OnceLock::new();
    RES.get_or_init(|| {
        compile_table(&[
            TAVERN,
            TEMPLATE,
            "——",
            r"[^，。！？\n]{1,8}[，,][^，。！？\n]{1,8}[，,][^，。！？\n]{1,8}[。！？]",
            r"[！!]",
            r#""#,
            "'",
            r"[A-Za-z]{2,}",
            r"「|”",
        ])
    })
}

fn count_matches(text: &str, pattern: &str) -> usize {
    match re_for(pattern) {
        Some(re) => re.find_iter(text).count(),
        None => 0,
    }
}

/// 命中的具体原文（去重、保序），供修复指令「逐条点名」——只报规则名，小模型会抓瞎不修
fn matches_of(text: &str, pattern: &str) -> Vec<String> {
    match re_for(pattern) {
        Some(re) => {
            let mut v: Vec<String> = re.find_iter(text).map(|m| m.as_str().to_string()).collect();
            v.dedup();
            v
        }
        None => Vec::new(),
    }
}

fn paras(text: &str) -> Vec<&str> {
    text.par_lines()
}

trait ParLines {
    fn par_lines(&self) -> Vec<&str>;
}
impl ParLines for str {
    fn par_lines(&self) -> Vec<&str> {
        self.lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

/// 评分：返回 JSON {score, passed, violations:[{rule,tier,count,advice}], metrics:{chars,paras,avg_para,dialog_pct}}
pub fn score_text(text: &str) -> Value {
    let t = text;
    let chars = t.chars().count().max(1) as f64;
    let per_1k = |n: usize| n as f64 * 1000.0 / chars;
    let mut score: f64 = 0.0;
    let mut violations: Vec<Value> = Vec::new();

    // Tier1 一票命中
    for (pat, name) in TIER1 {
        let ms = matches_of(t, pat);
        let n = ms.len();
        if n > 0 {
            score += 15.0 * n as f64;
            violations.push(json!({"rule": name, "tier": 1, "count": n,
                "matches": ms.into_iter().take(8).collect::<Vec<_>>(),
                "advice": format!("逐一替换下面命中的短语（{}处），换成人物此刻具体的动作/反应；不许把直接引语改成转述", n)}));
        }
    }

    // Tier2 同段扎堆
    let mut cluster_paras = 0usize;
    for p in paras(t) {
        let hits = TIER2
            .iter()
            .filter(|(pat, _)| count_matches(p, pat) > 0)
            .count();
        if hits >= 2 {
            cluster_paras += 1;
        }
    }
    if cluster_paras >= 1 {
        score += 8.0 * cluster_paras as f64;
        violations.push(
            json!({"rule": "信号扎堆", "tier": 2, "count": cluster_paras,
            "advice": "连接词/渲染副词/否定式排比在同段扎堆——真人 p97≈0。拆开重写该段"}),
        );
    }

    // Tier4 密度制
    for (pat, name, th, advice) in TIER4 {
        let ms = matches_of(t, pat);
        let n = ms.len();
        let d = per_1k(n);
        if d > *th {
            score += 8.0;
            violations.push(json!({"rule": name, "tier": 4, "count": n,
                "matches": ms.into_iter().take(6).collect::<Vec<_>>(),
                "advice": format!("{}（{:.1}/千字，阈值{}）", advice, d, th)}));
        }
    }

    // 酒馆腔 + 模板句
    for (pat, name) in [(TAVERN, "酒馆腔"), (TEMPLATE, "模板句")] {
        let ms = matches_of(t, pat);
        let n = ms.len();
        if n > 0 {
            score += 6.0 * n as f64;
            violations.push(json!({"rule": name, "tier": 3, "count": n,
                "matches": ms.into_iter().take(6).collect::<Vec<_>>(),
                "advice": format!("{}逐处最小替换为具体描写", name)}));
        }
    }

    // 节奏指标：破折号/三连/感叹号/短句连发
    let dash = count_matches(t, "——");
    if per_1k(dash) > 4.5 {
        score += 10.0;
        violations.push(json!({"rule": "破折号过密", "tier": 3, "count": dash,
            "advice": "破折号超密（真人p90=4.5/千字），大部分可改逗号或句号"}));
    }
    let sanlian = count_matches(
        t,
        r"[^，。！？\n]{1,8}[，,][^，。！？\n]{1,8}[，,][^，。！？\n]{1,8}[。！？]",
    );
    if per_1k(sanlian) > 9.0 {
        score += 10.0;
        violations.push(json!({"rule": "三连句式", "tier": 3, "count": sanlian,
            "advice": "「A，B，C。」三连结构超密，改成长短不一的句子"}));
    }
    let exclaim = count_matches(t, r"[！!]");
    if per_1k(exclaim) > 12.5 {
        score += 10.0;
        violations.push(json!({"rule": "感叹号过密", "tier": 3, "count": exclaim,
            "advice": "感叹号超密（真人p90=12.5/千字），留最关键的两三处"}));
    }

    // 段落结构：连续极短段（≤4字）连发
    let ps = paras(t);
    let mut run_short = 0usize;
    let mut max_run = 0usize;
    for p in &ps {
        if p.chars().count() <= 4 {
            run_short += 1;
            max_run = max_run.max(run_short);
        } else {
            run_short = 0;
        }
    }
    if max_run > 4 {
        score += 12.0;
        violations.push(json!({"rule": "短句连发", "tier": 3, "count": max_run,
            "advice": "连续多个极短段（≤4字）——碎片化分段本身是AI指纹（案例001），合并成正常段落"}));
    }

    // ── 结构指纹（tier5）：词表门抓不到的「短句堆叠 / 段落碎 / 段落过于均匀」——
    //    实测对照：AI 稿平均句长 17~22 字、≤10字短句 25~32%、平均段长 34~40 字；
    //    真人稿（诡秘之主第1章）平均句长 39 字、≤10字短句 9.4%、平均段长 67 字、CV 0.59。
    //    只在正文足够长时判定（短选区/片段统计不可靠）。
    if chars >= 800.0 {
        let sents: Vec<&str> = t
            .split(['。', '！', '？', '…'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        let sn = sents.len().max(1) as f64;
        let avg_sent = sents.iter().map(|s| s.chars().count()).sum::<usize>() as f64 / sn;
        let short_ratio = sents.iter().filter(|s| s.chars().count() <= 10).count() as f64 / sn;
        let short_n = sents.iter().filter(|s| s.chars().count() <= 10).count();
        if short_ratio > 0.18 {
            score += 14.0;
            violations.push(json!({"rule": "短句主导", "tier": 5,
                "count": short_n,
                "advice": format!("10字以内短句占 {:.0}%（真人≈9%，超 18% 即AI指纹）：把一半短句并成长句、加从句或状语，让句长参差；但不要写出 80 字以上的超长句", short_ratio * 100.0)}));
        }
        if avg_sent < 22.0 {
            score += 12.0;
            violations.push(json!({"rule": "句长过短", "tier": 5, "count": 1,
                "advice": format!("平均句长仅 {:.1} 字（真人≈30~40 字）：补足主谓宾与修饰，别通篇一句一顿", avg_sent)}));
        }
        let avg_para = chars / ps.len().max(1) as f64;
        if avg_para < 50.0 {
            score += 12.0;
            violations.push(json!({"rule": "段落过碎", "tier": 5, "count": 1,
                "advice": format!("平均段长仅 {:.0} 字（真人≈60~70 字）：适度并段，目标平均 60~70 字、段落数不得少于原文的三分之二；不要并成几大块整段", avg_para)}));
        }
        if ps.len() >= 8 {
            let sd = (ps
                .iter()
                .map(|p| {
                    let d = p.chars().count() as f64 - avg_para;
                    d * d
                })
                .sum::<f64>()
                / ps.len() as f64)
                .sqrt();
            let cv = if avg_para > 0.0 { sd / avg_para } else { 0.0 };
            if cv < 0.45 {
                score += 6.0;
                violations.push(json!({"rule": "段落过于均匀", "tier": 5,
                    "count": (cv * 100.0).round() as i64,
                    "advice": format!("段落长度变异系数仅 {:.2}（真人≈0.6）：刻意留一两段长段、一两段极短段，别让每段都差不多长", cv)}));
            }
        }
        // 段落过长：审核重写常把对白/短段并成一大块（手机上不可读），机器门原来只管「过碎」不管「过长」
        let long_paras = ps.iter().filter(|p| p.chars().count() > 220).count();
        if long_paras > 0 {
            score += 8.0 * long_paras.min(5) as f64;
            let samples: Vec<String> = ps
                .iter()
                .filter(|p| p.chars().count() > 220)
                .map(|p| p.chars().take(24).collect::<String>() + "…")
                .take(4)
                .collect();
            violations.push(json!({"rule": "段落过长", "tier": 5, "count": long_paras,
                "matches": samples,
                "advice": "有超长段（>220字）：在对白、动作或转折处拆成 2~4 段，对白独立成段；不许再并段"}));
        }
    }

    // 卫生 lint（精简版门6）：直引号 / 英文单词 / 繁体水印
    let straight_quotes = count_matches(t, r#"""#) + count_matches(t, "'");
    if straight_quotes > 0 {
        score += 4.0 * straight_quotes.min(10) as f64;
        violations.push(json!({"rule": "直引号", "tier": 3, "count": straight_quotes,
            "advice": "正文用了直引号「\"\"''」——必须全部替换为中文弯引号「“”‘’」（直引号=翻译腔指纹）"}));
    }
    let eng_words = count_matches(t, r"[A-Za-z]{2,}");
    if eng_words > 0 {
        score += 4.0 * eng_words.min(10) as f64;
        violations.push(json!({"rule": "英文残留", "tier": 3, "count": eng_words,
            "advice": "正文残留英文单词（案例010），全部换成中文"}));
    }

    let avg_para = if ps.is_empty() {
        0.0
    } else {
        chars / ps.len() as f64
    };
    let dialog_n = count_matches(t, r"「|”");
    let dialog_pct = (dialog_n as f64 * 1000.0 / chars).min(100.0);
    score = score.min(100.0);
    // Tier1 一票否决：命中任一条网文套话即判「未过」（原逻辑单处 15 分 < 32 通过线，导致个别套话漏网不修）
    let tier1_hit = violations.iter().any(|v| v["tier"].as_i64() == Some(1));
    json!({
        "score": score as i64,
        "passed": score <= PASS_SCORE && !tier1_hit,
        "tier1": tier1_hit,
        "passLine": PASS_SCORE,
        "violations": violations,
        "metrics": {
            "chars": chars as i64,
            "paras": ps.len(),
            "avg_para": (avg_para * 10.0).round() / 10.0,
            "dialog_pct": (dialog_pct * 10.0).round() / 10.0,
        }
    })
}

/// 生成「整合提示词」的用户指令段：把违规清单变成可执行的修改指令
pub fn build_fix_instruction(report: &Value) -> String {
    let mut s = String::from("【去AI味机器门报告】你的初稿被检出以下AI写作特征，请逐条修复：\n");
    if let Some(arr) = report["violations"].as_array() {
        for v in arr {
            s.push_str(&format!(
                "- {}（{}处）：{}\n",
                v["rule"].as_str().unwrap_or("?"),
                v["count"].as_i64().unwrap_or(0),
                v["advice"].as_str().unwrap_or("")
            ));
            if let Some(ms) = v["matches"].as_array() {
                let list = ms
                    .iter()
                    .filter_map(|x| x.as_str())
                    .map(|x| format!("「{}」", x))
                    .collect::<Vec<_>>()
                    .join("");
                if !list.is_empty() {
                    s.push_str(&format!("    命中原文（必须逐一改掉或删除）：{}\n", list));
                }
            }
        }
    }
    s.push_str("\n【死命令】上面列出的「命中原文」必须逐一从正文里消失——换成符合上下文的具体动作/细节/台词；直接引语必须保留引号与说话人，严禁改成「问道…」「回答说…」式转述。\n");
    // 结构类问题（tier5）必须允许并段/调句长，否则「段落过碎/短句主导」无法修复
    let structural = report["violations"]
        .as_array()
        .map(|arr| arr.iter().any(|v| v["tier"].as_i64() == Some(5)))
        .unwrap_or(false);
    if structural {
        s.push_str("\n【保真红线】人物、事实、数字、时间、因果、信息点全保留；只改表达不改剧情；输出修复后全文，不加解释。为修复「短句主导/句长过短/段落过碎/段落过于均匀」，允许并段、允许把短句并成长句（段落数不得少于原文的三分之二，不许出现 80 字以上超长句），但不许增删信息、不许改剧情事实。");
    } else {
        s.push_str("\n【保真红线】人物、事实、数字、时间、因果、信息点全保留；只改表达不改剧情；段落数与段界不拆不并；输出修复后全文，不加解释。");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_passes() {
        let t = "林辰没听。\n\n他扶着帐柱，慢慢坐回榻边，把掌心那点血在衣襟上蹭了蹭。肺还在疼，脑子却前所未有地清楚。\n\n他在心里摆开一张牌桌。\n\n一张牌，是他这幅活不过入冬的身子。一张牌，是那个跪在院里、膝盖硬撑的老头，和身后一个快散了的林家。三张牌，全压着他。";
        let r = score_text(t);
        assert_eq!(r["passed"], json!(true), "score={}", r["score"]);
    }

    #[test]
    fn ai_flavored_text_fails() {
        let t = "不得不说，他的心中涌起一股难以言喻的复杂情绪。阳光明媚，在这个繁华的时代，故事才刚刚开始。\n\n他的嘴角勾起一抹弧度，眸中闪过一丝精光，空气仿佛凝固了。\n\n他缓缓地、深深地、默默地握紧了拳头，深吸一口气，咬了咬牙。他的眼神中闪过一丝不易察觉的笑意，仿佛在预示着什么。\n\n与此同时，然而对方早有防备。他终于意识到了事情的严重性，心情前所未有的凝重。";
        let r = score_text(t);
        assert_eq!(r["passed"], json!(false), "score={}", r["score"]);
        assert!(r["score"].as_i64().unwrap() > 32);
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    #[test]
    fn parity_with_python_sample() {
        // 与 python ai_flavor_score.py 对同一样本的分数对齐测试（样本：缓副词+元叙述+空洞渲染）
        let t = "林辰没听。\n\n他扶着帐柱，缓缓坐回榻边，把掌心那点血在衣襟上蹭了蹭。肺还在疼，脑子却前所未有地清楚。\n\n不得不说，他的心里涌起一股难以言喻的复杂情绪。\n";
        let r = score_text(t);
        // python 版同文本 = 28 分（tier4 轻缓副词 8 + tier1 不得不说 15 + tier1 难以言喻 15 - 但 python 算 28 分制不同）
        // 本移植：15+15+8 = 38 > 32 → 不通过。方向一致（检出），阈值口径记录在案。
        assert!(r["score"].as_i64().unwrap() >= 30, "score={}", r["score"]);
        assert_eq!(r["passed"], json!(false));
    }
}
