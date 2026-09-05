use teloxide::types::BotCommand;
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "支持的命令:")]
pub enum Command {
    #[command(description = "显示帮助信息")]
    Help,
    #[command(description = "[仅Admin私聊] 查看 Bot 状态信息")]
    Info,
    #[command(description = "订阅作者\n  用法: /sub [ch=<频道ID>] <id,...> [+tag1 -tag2]")]
    Sub(String),
    #[command(description = "订阅排行榜\n  用法: /subrank [ch=<频道ID>] <mode>")]
    SubRank(String),
    #[command(description = "取消订阅作者\n  用法: /unsub [ch=<频道ID>] <author_id,...>")]
    Unsub(String),
    #[command(description = "取消订阅排行榜\n  用法: /unsubrank [ch=<频道ID>] <mode>")]
    UnsubRank(String),
    #[command(description = "回复消息取消对应订阅")]
    UnsubThis,
    #[command(description = "列出当前订阅\n  用法: /list [ch=<频道ID>]")]
    List(String),
    #[command(description = "[仅Owner] 设置用户为管理员\n  用法: /setadmin <user_id>")]
    SetAdmin(String),
    #[command(description = "[仅Owner] 移除用户管理员角色\n  用法: /unsetadmin <user_id>")]
    UnsetAdmin(String),
    #[command(description = "[仅Admin] 启用聊天\n  用法: /enablechat [chat_id]")]
    EnableChat(String),
    #[command(description = "[仅Admin] 禁用聊天\n  用法: /disablechat [chat_id]")]
    DisableChat(String),
    #[command(description = "显示和管理聊天设置")]
    Settings,
    #[command(description = "下载作品原图\n  用法: /download <url|id> 或回复消息")]
    Download(String),
    #[command(description = "订阅 Booru 标签\n  用法: /bsub [ch=<频道ID>] <站点:标签> [过滤条件]")]
    BSub(String),
    #[command(description = "取消 Booru 标签订阅\n  用法: /bunsub [ch=<频道ID>] <站点:标签>")]
    BUnsub(String),
    #[command(description = "订阅 Booru 排行榜: <站点:> scale=day|week|month [过滤条件]")]
    BRank(String),
    #[command(description = "订阅 Booru 日榜: <站点:> [过滤条件]")]
    BRankDay(String),
    #[command(description = "订阅 Booru 周榜: <站点:> [过滤条件]")]
    BRankWeek(String),
    #[command(description = "订阅 Booru 月榜: <站点:> [过滤条件]")]
    BRankMonth(String),
    #[command(description = "订阅 Booru 随机推送: <站点:间隔> [过滤条件]  间隔格式: 1h/2h30m/30m")]
    BRand(String),
    #[command(description = "订阅 E-Hentai 画廊\n  用法: /esub [ch=<频道ID>] <搜索词> [过滤条件]")]
    ESub(String),
    #[command(description = "取消 E-Hentai 订阅\n  用法: /eunsub [ch=<频道ID>] <搜索词>")]
    EUnsub(String),
    #[command(description = "直接下载 E-Hentai 画廊\n  用法: /edl <url> [telegraph=on]")]
    EDl(String),
    #[command(description = "查看当前聊天的 E-Hentai 下载队列", parse_with = "split")]
    EStatus {},
    #[command(
        description = "下载 E-Hentai 画廊并上传 Telegraph\n  用法: /telegraph <url> 或回复消息"
    )]
    Telegraph(String),
    #[command(description = "取消当前设置操作")]
    Cancel,
}

impl Command {
    /// 获取普通用户可见的命令列表
    pub fn user_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
        let mut commands = vec![
            BotCommand::new("sub", "订阅作者 - /sub [ch=<频道ID>] <id,...>"),
            BotCommand::new("subrank", "订阅排行榜 - /subrank [ch=<频道ID>] <mode>"),
            BotCommand::new("list", "列出当前订阅 - /list [ch=<频道ID>]"),
            BotCommand::new("unsub", "取消订阅作者 - /unsub [ch=<频道ID>] <id,...>"),
            BotCommand::new(
                "unsubrank",
                "取消订阅排行榜 - /unsubrank [ch=<频道ID>] <mode>",
            ),
            BotCommand::new("unsubthis", "回复消息取消对应订阅"),
            BotCommand::new("settings", "显示和管理聊天设置"),
            BotCommand::new("download", "下载作品原图 - /download <url|id> 或回复消息"),
        ];

        if has_booru {
            commands.extend([
                BotCommand::new("bsub", "订阅Booru标签 - /bsub <站点:标签> [过滤条件]"),
                BotCommand::new("bunsub", "取消Booru标签订阅 - /bunsub <站点:标签>"),
                BotCommand::new(
                    "brank",
                    "订阅Booru排行榜 - /brank <站点:> scale=day|week|month [+tag -tag]",
                ),
                BotCommand::new("brankday", "订阅Booru日榜 - /brankday <站点:> [+tag -tag]"),
                BotCommand::new(
                    "brankweek",
                    "订阅Booru周榜 - /brankweek <站点:> [+tag -tag]",
                ),
                BotCommand::new(
                    "brankmonth",
                    "订阅Booru月榜 - /brankmonth <站点:> [+tag -tag]",
                ),
                BotCommand::new(
                    "brand",
                    "订阅Booru随机推送 - /brand <站点:间隔> [过滤条件]  间隔格式: 1h/2h30m/30m",
                ),
            ]);
        }

        if has_ehentai {
            commands.extend([
                BotCommand::new("esub", "订阅EH画廊 - /esub <搜索词> [过滤条件]"),
                BotCommand::new("eunsub", "取消EH订阅 - /eunsub <搜索词>"),
                BotCommand::new("edl", "下载EH画廊 - /edl <url> [telegraph=on]"),
                BotCommand::new("estatus", "查看当前聊天的EH下载队列"),
                BotCommand::new(
                    "telegraph",
                    "下载EH画廊上传Telegraph - /telegraph <url> 或回复消息",
                ),
            ]);
        }

        commands.push(BotCommand::new("help", "显示帮助信息"));

        commands
    }

    /// 获取管理员可见的命令列表（包含普通命令 + 管理员命令）
    pub fn admin_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
        let mut cmds = Self::user_commands(has_booru, has_ehentai);
        cmds.extend([
            BotCommand::new("info", "[Admin] 查看 Bot 状态信息"),
            BotCommand::new("enablechat", "[Admin] 启用聊天 - /enablechat [chat_id]"),
            BotCommand::new("disablechat", "[Admin] 禁用聊天 - /disablechat [chat_id]"),
        ]);
        cmds
    }

    /// 获取 Owner 可见的完整命令列表（包含所有命令）
    pub fn owner_commands(has_booru: bool, has_ehentai: bool) -> Vec<BotCommand> {
        let mut cmds = Self::admin_commands(has_booru, has_ehentai);
        cmds.extend([
            BotCommand::new("setadmin", "[Owner] 设置管理员 - /setadmin <user_id>"),
            BotCommand::new("unsetadmin", "[Owner] 移除管理员 - /unsetadmin <user_id>"),
        ]);
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::Command;

    fn command_names(commands: Vec<teloxide::types::BotCommand>) -> Vec<String> {
        commands
            .into_iter()
            .map(|command| command.command)
            .collect()
    }

    #[test]
    fn command_visibility_follows_provider_configuration_and_role() {
        for (booru, eh) in [(false, false), (true, false), (false, true), (true, true)] {
            for (role, commands) in [
                ("user", Command::user_commands(booru, eh)),
                ("admin", Command::admin_commands(booru, eh)),
                ("owner", Command::owner_commands(booru, eh)),
            ] {
                let names = command_names(commands);
                for (provider_commands, visible) in [
                    (
                        &[
                            "bsub",
                            "bunsub",
                            "brank",
                            "brankday",
                            "brankweek",
                            "brankmonth",
                            "brand",
                        ][..],
                        booru,
                    ),
                    (&["esub", "eunsub", "edl", "estatus"][..], eh),
                ] {
                    for command in provider_commands {
                        assert_eq!(
                            names.iter().any(|name| name == command),
                            visible,
                            "{role}: {command}, booru={booru}, eh={eh}"
                        );
                    }
                }
                if role == "admin" {
                    assert!(names.iter().any(|name| name == "info"));
                }
                if role == "owner" {
                    assert!(names.iter().any(|name| name == "setadmin"));
                }
            }
        }
    }
}
