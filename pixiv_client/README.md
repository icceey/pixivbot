# pixiv_client

轻量级 Pixiv API 客户端，专为本项目需求实现。

## 致谢

本模块的设计和实现参考了 [pixivpy](https://github.com/upbit/pixivpy) 项目。

**原项目**: [upbit/pixivpy](https://github.com/upbit/pixivpy)  
**作者**: @upbit  
**许可**: Unlicense

感谢 @upbit 的辛苦付出，为 Pixiv API 封装提供了优秀的参考实现。

## 实现说明

本模块根据项目需求，仅实现了以下核心功能：

- OAuth 认证 (`auth.rs`)
- 获取画师作品列表 (`user_illusts`)
- 获取作品详情 (`illust_detail`)
- 获取排行榜 (`illust_ranking`)

使用 `reqwest` 作为 HTTP 客户端，提供异步 API 调用。

## 文件结构

```
pixiv_client/
├── mod.rs      # 模块定义
├── error.rs    # 错误类型
├── models.rs   # 数据模型
├── auth.rs     # OAuth 认证
└── client.rs   # API 客户端
```
