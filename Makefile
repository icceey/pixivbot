# Makefile for pixivbot
# 用于本地开发时快速检查是否符合 CI 要求

.PHONY: all check test fmt fmt-check clippy build clean ci help

# 默认目标：运行所有 CI 检查
all: ci

# 帮助信息
help:
	@echo "Available targets:"
	@echo "  make all       - Run all CI checks (same as 'make ci')"
	@echo "  make ci        - Run all CI checks: fmt-check, clippy, check, test, build"
	@echo "  make check     - Run cargo check"
	@echo "  make test      - Run cargo test"
	@echo "  make fmt       - Format code with rustfmt"
	@echo "  make fmt-check - Check code formatting (CI mode)"
	@echo "  make clippy    - Run clippy linter (CI mode with -D warnings)"
	@echo "  make build     - Build release binary"
	@echo "  make clean     - Clean build artifacts"
	@echo "  make dev       - Run in development mode"
	@echo "  make watch     - Watch for changes and rebuild"

# 与 CI 一致的环境变量
RUSTFLAGS := -Dwarnings

# 检查代码格式 (CI)
fmt-check:
	@echo "🔍 Checking code formatting..."
	cargo fmt --all -- --check

# 格式化代码
fmt:
	@echo "✨ Formatting code..."
	cargo fmt --all

# cargo check
check:
	@echo "🔍 Running cargo check..."
	cargo check --workspace --all-targets

# 运行测试
test:
	@echo "🧪 Running tests..."
	cargo test --workspace --all-targets

# Clippy 检查 (CI 模式)
clippy:
	@echo "📎 Running clippy..."
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --workspace --all-targets -- -D warnings

# 构建 release 版本
build:
	@echo "🔨 Building release..."
	cargo build --release --workspace --features ffmpeg-codec

# 清理构建产物
clean:
	@echo "🧹 Cleaning..."
	cargo clean

# 开发模式运行
dev:
	@echo "🚀 Running in development mode..."
	cargo run

# 监听文件变化并重新构建 (需要 cargo-watch)
watch:
	@echo "👀 Watching for changes..."
	cargo watch -x run

# 运行所有 CI 检查 (与 GitHub Actions 一致)
ci: fmt-check clippy check test build
	@echo ""
	@echo "✅ All CI checks passed!"
	@echo ""

# 快速检查 (不包含完整构建)
quick: fmt-check clippy check
	@echo ""
	@echo "✅ Quick checks passed!"
	@echo ""

# 修复常见问题
fix:
	@echo "🔧 Fixing code issues..."
	cargo fmt --all
	cargo clippy --fix --allow-dirty --allow-staged --workspace --all-targets
