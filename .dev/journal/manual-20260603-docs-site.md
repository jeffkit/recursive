# Manual edit: docs-site

**Date**: 2026-06-03
**Goal**: 为 Recursive 项目创建 VitePress 文档站，中英双语，部署到 GitHub Pages
**Files touched**:
- website/ (新增，完整 VitePress 站点)
  - .vitepress/config.ts (双语配置)
  - en/ (英文文档：guide、cli、library、http-api、sdk、deployment、multi-agent)
  - zh/ (中文文档，镜像英文结构)
  - public/logo.svg, favicon.svg
  - package.json, pnpm-lock.yaml
- .github/workflows/docs.yml (新增，自动构建部署到 GitHub Pages)

**Tests added**: none (文档站不含运行时逻辑)

**Notes**:
- 使用 VitePress 1.6.4，pnpm 管理依赖
- package.json 需要 "type": "module" 和 pnpm.onlyBuiltDependencies: ["esbuild"]
- base URL 设为 /recursive/ 以匹配 GitHub Pages 路径
- 已通过 gh API 启用 GitHub Pages（workflow 模式），首次部署已成功
- 最终 URL: https://jeffkit.github.io/recursive/
