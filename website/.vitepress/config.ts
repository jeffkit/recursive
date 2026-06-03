import { defineConfig } from 'vitepress'

const enNav = [
  { text: 'Guide', link: '/en/guide/' },
  { text: 'CLI', link: '/en/cli/' },
  { text: 'Library API', link: '/en/library/' },
  { text: 'HTTP API', link: '/en/http-api/' },
  { text: 'SDK', link: '/en/sdk/python' },
  { text: 'Deployment', link: '/en/deployment/' },
  { text: 'Changelog', link: '/en/changelog' },
]

const zhNav = [
  { text: '指南', link: '/zh/guide/' },
  { text: 'CLI', link: '/zh/cli/' },
  { text: '库 API', link: '/zh/library/' },
  { text: 'HTTP API', link: '/zh/http-api/' },
  { text: 'SDK', link: '/zh/sdk/python' },
  { text: '部署', link: '/zh/deployment/' },
  { text: '更新日志', link: '/zh/changelog' },
]

const enSidebar = {
  '/en/guide/': [
    {
      text: 'Getting Started',
      items: [
        { text: 'Introduction', link: '/en/guide/' },
        { text: 'Quick Start', link: '/en/guide/quickstart' },
        { text: 'Core Concepts', link: '/en/guide/concepts' },
        { text: 'Configuration', link: '/en/guide/config' },
      ],
    },
  ],
  '/en/cli/': [
    {
      text: 'CLI Reference',
      items: [
        { text: 'Overview', link: '/en/cli/' },
        { text: 'run', link: '/en/cli/run' },
        { text: 'repl', link: '/en/cli/repl' },
        { text: 'loop', link: '/en/cli/loop' },
        { text: 'http', link: '/en/cli/http' },
        { text: 'tools', link: '/en/cli/tools' },
        { text: 'sessions', link: '/en/cli/sessions' },
      ],
    },
  ],
  '/en/library/': [
    {
      text: 'Library API',
      items: [
        { text: 'Overview', link: '/en/library/' },
        { text: 'Agent Builder', link: '/en/library/agent' },
        { text: 'Custom Tools', link: '/en/library/tools' },
        { text: 'Custom Providers', link: '/en/library/providers' },
        { text: 'Events & Observers', link: '/en/library/events' },
        { text: 'Multi-Agent', link: '/en/library/multi-agent' },
      ],
    },
  ],
  '/en/http-api/': [
    {
      text: 'HTTP API',
      items: [
        { text: 'Overview', link: '/en/http-api/' },
        { text: 'Sessions', link: '/en/http-api/sessions' },
        { text: 'Run & Stream', link: '/en/http-api/run' },
        { text: 'Tools', link: '/en/http-api/tools' },
        { text: 'OpenAPI Spec', link: '/en/http-api/openapi' },
      ],
    },
  ],
  '/en/sdk/': [
    {
      text: 'SDK',
      items: [
        { text: 'Python SDK', link: '/en/sdk/python' },
        { text: 'TypeScript SDK', link: '/en/sdk/typescript' },
      ],
    },
  ],
  '/en/deployment/': [
    {
      text: 'Deployment',
      items: [
        { text: 'Overview', link: '/en/deployment/' },
        { text: 'Docker', link: '/en/deployment/docker' },
        { text: 'Cloud (Redis + S3)', link: '/en/deployment/cloud' },
        { text: 'Sandbox Modes', link: '/en/deployment/sandbox' },
      ],
    },
  ],
}

const zhSidebar = {
  '/zh/guide/': [
    {
      text: '开始使用',
      items: [
        { text: '介绍', link: '/zh/guide/' },
        { text: '快速开始', link: '/zh/guide/quickstart' },
        { text: '核心概念', link: '/zh/guide/concepts' },
        { text: '配置参考', link: '/zh/guide/config' },
      ],
    },
  ],
  '/zh/cli/': [
    {
      text: 'CLI 参考',
      items: [
        { text: '概览', link: '/zh/cli/' },
        { text: 'run', link: '/zh/cli/run' },
        { text: 'repl', link: '/zh/cli/repl' },
        { text: 'loop', link: '/zh/cli/loop' },
        { text: 'http', link: '/zh/cli/http' },
        { text: 'tools', link: '/zh/cli/tools' },
        { text: 'sessions', link: '/zh/cli/sessions' },
      ],
    },
  ],
  '/zh/library/': [
    {
      text: '库 API',
      items: [
        { text: '概览', link: '/zh/library/' },
        { text: 'Agent 构建器', link: '/zh/library/agent' },
        { text: '自定义 Tool', link: '/zh/library/tools' },
        { text: '自定义 Provider', link: '/zh/library/providers' },
        { text: '事件与观察者', link: '/zh/library/events' },
        { text: '多 Agent', link: '/zh/library/multi-agent' },
      ],
    },
  ],
  '/zh/http-api/': [
    {
      text: 'HTTP API',
      items: [
        { text: '概览', link: '/zh/http-api/' },
        { text: 'Sessions', link: '/zh/http-api/sessions' },
        { text: 'Run & Stream', link: '/zh/http-api/run' },
        { text: 'Tools', link: '/zh/http-api/tools' },
        { text: 'OpenAPI 规范', link: '/zh/http-api/openapi' },
      ],
    },
  ],
  '/zh/sdk/': [
    {
      text: 'SDK',
      items: [
        { text: 'Python SDK', link: '/zh/sdk/python' },
        { text: 'TypeScript SDK', link: '/zh/sdk/typescript' },
      ],
    },
  ],
  '/zh/deployment/': [
    {
      text: '部署',
      items: [
        { text: '概览', link: '/zh/deployment/' },
        { text: 'Docker', link: '/zh/deployment/docker' },
        { text: '云端部署（Redis + S3）', link: '/zh/deployment/cloud' },
        { text: '沙箱模式', link: '/zh/deployment/sandbox' },
      ],
    },
  ],
}

export default defineConfig({
  title: 'Recursive',
  description: 'A minimal, orthogonal, embeddable coding agent kernel in Rust',

  head: [
    ['link', { rel: 'icon', href: '/recursive/favicon.svg' }],
  ],

  base: '/recursive/',

  locales: {
    root: {
      label: 'English',
      lang: 'en-US',
    },
    zh: {
      label: '中文',
      lang: 'zh-CN',
      description: '极简、正交、可嵌入的 Rust 编码 Agent 内核',
      themeConfig: {
        nav: zhNav,
        sidebar: zhSidebar,
        outline: {
          label: '本页目录',
        },
        docFooter: {
          prev: '上一页',
          next: '下一页',
        },
        darkModeSwitchLabel: '主题',
        sidebarMenuLabel: '目录',
        returnToTopLabel: '回到顶部',
        langMenuLabel: '切换语言',
      },
    },
  },

  themeConfig: {
    logo: '/logo.svg',
    nav: enNav,
    sidebar: { ...enSidebar, ...zhSidebar },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/jeffkit/recursive' },
    ],

    editLink: {
      pattern: 'https://github.com/jeffkit/recursive/edit/main/website/:path',
      text: 'Edit this page on GitHub',
    },

    footer: {
      message: 'Released under the MIT License.',
      copyright: 'Copyright © 2024-present Jeff Kit',
    },

    search: {
      provider: 'local',
    },
  },
})
