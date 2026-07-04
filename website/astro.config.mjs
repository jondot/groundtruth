import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://getgt.vercel.app',
  output: 'static',
  integrations: [
    starlight({
      title: 'groundtruth',
      description: 'Database health monitoring in a single binary',
      logo: {
        light: './src/assets/logo-light.svg',
        dark: './src/assets/logo-dark.svg',
        replacesTitle: false,
      },
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/jondot/groundtruth' },
      ],
      sidebar: [
        {
          label: 'Getting Started',
          items: [
            { label: 'Introduction', slug: 'docs/introduction' },
            { label: 'Install', slug: 'docs/install' },
            { label: 'Quick Start', slug: 'docs/quickstart' },
          ],
        },
        {
          label: 'Concepts',
          items: [
            { label: 'How checks work', slug: 'docs/concepts' },
          ],
        },
        {
          label: 'Guides',
          items: [
            { label: 'Writing checks', slug: 'docs/writing-checks' },
            { label: 'Validating column data', slug: 'docs/data-validation' },
            { label: 'Alerting', slug: 'docs/alerting' },
            { label: 'Securing & persisting', slug: 'docs/securing-and-persisting' },
            { label: 'Integrating', slug: 'docs/integrating' },
            { label: 'Deploying', slug: 'docs/deploy' },
            { label: 'Using with AI agents', slug: 'docs/mcp' },
          ],
        },
        {
          label: 'Reference',
          items: [
            { label: 'CLI', slug: 'docs/cli' },
            { label: 'Configuration', slug: 'docs/configuration' },
            { label: 'HTTP API & metrics', slug: 'docs/http-api' },
            { label: 'Limitations & scope', slug: 'docs/limitations' },
          ],
        },
      ],
      customCss: ['./src/styles/custom.css'],
    }),
  ],
});
