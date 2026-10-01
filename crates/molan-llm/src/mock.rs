//! mock:// 确定性假响应（端到端测试 / 离线演示）：按系统提示中的唯一标识返回最小可用内容。
//! 这些是协议/流程验证用的固定文本，不代表任何真实模型的输出质量。

/// 根据系统提示（与最后一条用户消息）选择固定回复。
pub(crate) fn mock_body(sys: &str, user: &str) -> String {
    if sys.contains("审读一章正文") {
        // 审核员：返回「通过」的合法审核 JSON（否则 fail-closed 永远走不到审核通过分支）
        return "{\"ok\":true,\"issues\":[],\"fix\":\"\"}".to_string();
    }
    if sys.contains("小说连续性复核员") {
        // 记忆语义复核：确定性通过（mock 不评判忠实度，真实模型才能验证质量）
        return "{\"passed\":true,\"errors\":[]}".to_string();
    }
    if sys.contains("小说连续性记录员") {
        // 章节记忆抽取：证据必须逐字取自原文——取原文第一段正文的前若干字，不编造内容
        let evidence: String = user
            .lines()
            .map(str::trim)
            .find(|l| l.chars().count() >= 8 && !l.starts_with('【') && !l.starts_with('#'))
            .map(|l| l.chars().take(14).collect())
            .unwrap_or_default();
        return serde_json::json!({
            "summary": "本章为测试章节，情节以原文为准。",
            "facts": [], "threads": [],
            "events": [{"description": "本章开篇情节", "evidence": evidence}],
        })
        .to_string();
    }
    if sys.contains("输出人物状态变更 JSON")
        || sys.contains("只输出 JSON")
        || sys.contains("只输出JSON")
    {
        return "{\"updates\":[],\"newCharacters\":[]}".to_string();
    }
    if sys.contains("压缩成「前情摘要」") {
        return "（覆盖至第N章）主角获得入门名额，结识神秘老者，玉佩伏笔已埋（第1章）。"
            .to_string();
    }
    if sys.contains("走向级细纲") {
        return "# 第1章 细纲\n\n**本章目标**：主角登场，埋下身世伏笔。\n**冲突与对手**：与同门争夺入门名额。\n**看点与爽点**：主角逆袭拿到名额。\n**章末钩子**：神秘老者递来半块玉佩。".to_string();
    }
    // 正文：凑够 600+ 字避免被「正文过短」校验拦截
    let mut t = String::from("第一章 初入山门\n\n山风掠过石阶，少年背着一个旧行囊，站在了青岚宗的山门前。他抬头望着云雾深处的连绵殿宇，攥紧了手里的荐书。");
    for i in 0..8 {
        t.push_str(&format!(
            "\n\n这段路他走得并不轻松。第{}次停下歇脚的时候，他想起临行前村里的老人说过的话：修行一途，如逆水行舟，不进则退。少年咬了咬牙，继续向上走去。石阶尽头，一名灰袍弟子拦住了他，问他可有名录在册。少年递上荐书，对方翻看片刻，露出一丝讶异的神色，随即领着他往偏殿走去。",
            i + 1
        ));
    }
    t.push_str("\n\n偏殿之中，一位白发长老睁开双眼，目光如电，上下打量着这个风尘仆仆的少年。「倒是块好料子。」长老淡淡说道，「不过，我青岚宗收徒，向来只看根骨与心性。你可敢接下三试？」少年挺直脊背，朗声答道：「敢。」");
    t
}
