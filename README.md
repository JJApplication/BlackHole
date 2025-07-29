# 资源文件代理转发程序

这是一个使用Rust语言编写的资源文件请求代理转发程序，支持本地静态文件服务和unpkg代理转发功能。

## 功能特性

1. **本地静态文件服务**：对于不包含版本号的请求（如 `/static/github.css`），直接从本地 `static` 目录提供文件服务。

2. **unpkg代理转发**：对于符合unpkg格式的请求（如 `/static/vue@3.2.0/dist/vue.global.min.js`），会转发到unpkg.com获取文件并缓存到本地。

3. **智能缓存**：从unpkg下载的文件会缓存到本地，下次相同请求时优先使用缓存。

4. **TOML配置**：使用TOML配置文件控制代理功能的开启/关闭。

## 配置文件

配置文件 `config.toml` 包含以下选项：

```toml
[proxy]
# 是否启用代理功能，默认为false
enabled = false
# 本地静态文件目录
static_dir = "./static"
# unpkg缓存目录
cache_dir = "./cache"

[server]
# 监听端口
port = 8080
# 监听地址
host = "localhost"
```

## 使用方法

1. **启动服务器**：
   ```bash
   cargo run
   ```

2. **启用代理功能**：
   修改 `config.toml` 文件，将 `proxy.enabled` 设置为 `true`

3. **测试本地文件**：
   ```
   http://localhost:8080/static/github.css
   ```

4. **测试unpkg代理**：
   ```
   http://localhost:8080/static/vue@3.2.0/dist/vue.global.min.js
   ```

## 代理规则

### 规则1：本地静态文件
- 请求格式：`/static/filename.ext`（不包含@版本号）
- 示例：`/static/github.css`、`/static/app.js`
- 行为：直接从本地 `static` 目录查找并返回文件

### 规则2：unpkg代理转发
- 请求格式：`/static/:package@:version/:file`
- 示例：`/static/vue@3.2.0/dist/vue.global.min.js`
- 行为：
  1. 首先检查本地缓存
  2. 如果缓存不存在，从 `https://unpkg.com/:package@:version/:file` 下载
  3. 将下载的文件缓存到本地 `cache` 目录
  4. 返回文件内容

## 目录结构

```
.
├── Cargo.toml        # Rust项目配置文件
├── src/
│   └── main.rs       # 主程序文件
├── config.toml       # 配置文件
├── static/           # 本地静态文件目录
│   └── github.css    # 示例CSS文件
├── cache/            # unpkg缓存目录（自动创建）
└── README.md         # 说明文档
```

## 依赖

- `axum` - 现代化的Rust web框架
- `tokio` - 异步运行时
- `reqwest` - HTTP客户端
- `toml` - TOML配置文件解析
- `regex` - 正则表达式
- `tracing` - 日志记录

## 注意事项

1. 默认情况下代理功能是关闭的，需要在配置文件中启用
2. 确保有网络连接以访问unpkg.com
3. 缓存文件会保存在 `cache` 目录中，可以手动清理
4. 支持常见的文件类型Content-Type设置（CSS、JS、JSON、HTML、图片等）