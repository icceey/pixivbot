// Pixiv domain models
use serde::{Deserialize, Serialize};

/// 排行榜模式枚举
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RankingMode {
    /// 日榜
    Day,
    /// 周榜
    Week,
    /// 月榜
    Month,
    /// 男性日榜
    DayMale,
    /// 女性日榜
    DayFemale,
    /// 原创周榜
    WeekOriginal,
    /// 新人周榜
    WeekRookie,
    /// 漫画日榜
    DayManga,
    /// R18日榜
    DayR18,
    /// R18周榜
    WeekR18,
    /// R18G周榜
    WeekR18g,
    /// R18男性日榜
    DayMaleR18,
    /// R18女性日榜
    DayFemaleR18,
}

impl RankingMode {
    /// 获取排行榜模式的字符串表示（用于API调用）
    pub fn as_str(&self) -> &'static str {
        match self {
            RankingMode::Day => "day",
            RankingMode::Week => "week",
            RankingMode::Month => "month",
            RankingMode::DayMale => "day_male",
            RankingMode::DayFemale => "day_female",
            RankingMode::WeekOriginal => "week_original",
            RankingMode::WeekRookie => "week_rookie",
            RankingMode::DayManga => "day_manga",
            RankingMode::DayR18 => "day_r18",
            RankingMode::WeekR18 => "week_r18",
            RankingMode::WeekR18g => "week_r18g",
            RankingMode::DayMaleR18 => "day_male_r18",
            RankingMode::DayFemaleR18 => "day_female_r18",
        }
    }

    /// 获取排行榜模式的友好显示名称
    pub fn display_name(&self) -> &'static str {
        match self {
            RankingMode::Day => "日榜",
            RankingMode::Week => "周榜",
            RankingMode::Month => "月榜",
            RankingMode::DayMale => "男性日榜",
            RankingMode::DayFemale => "女性日榜",
            RankingMode::WeekOriginal => "原创周榜",
            RankingMode::WeekRookie => "新人周榜",
            RankingMode::DayManga => "漫画日榜",
            RankingMode::DayR18 => "R18日榜",
            RankingMode::WeekR18 => "R18周榜",
            RankingMode::WeekR18g => "R18G周榜",
            RankingMode::DayMaleR18 => "R18男性日榜",
            RankingMode::DayFemaleR18 => "R18女性日榜",
        }
    }

    /// 从字符串解析排行榜模式
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "day" => Some(RankingMode::Day),
            "week" => Some(RankingMode::Week),
            "month" => Some(RankingMode::Month),
            "day_male" => Some(RankingMode::DayMale),
            "day_female" => Some(RankingMode::DayFemale),
            "week_original" => Some(RankingMode::WeekOriginal),
            "week_rookie" => Some(RankingMode::WeekRookie),
            "day_manga" => Some(RankingMode::DayManga),
            "day_r18" => Some(RankingMode::DayR18),
            "week_r18" => Some(RankingMode::WeekR18),
            "week_r18g" => Some(RankingMode::WeekR18g),
            "day_male_r18" => Some(RankingMode::DayMaleR18),
            "day_female_r18" => Some(RankingMode::DayFemaleR18),
            _ => None,
        }
    }

    /// 获取所有有效的排行榜模式
    pub fn all_modes() -> Vec<&'static str> {
        vec![
            "day",
            "week",
            "month",
            "day_male",
            "day_female",
            "week_original",
            "week_rookie",
            "day_manga",
            "day_r18",
            "week_r18",
            "week_r18g",
            "day_male_r18",
            "day_female_r18",
        ]
    }
}
