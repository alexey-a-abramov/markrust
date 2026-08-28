export interface DocLink {
  title: string;
  href: string;
}

export interface DocSection {
  title: string;
  links: DocLink[];
}

export const docsNav: DocSection[] = [
  {
    title: 'Getting Started',
    links: [
      { title: 'Introduction', href: '/docs' },
      { title: 'Installation', href: '/docs/install' },
      { title: 'First document', href: '/docs/first-document' },
      { title: 'Opening a workspace', href: '/docs/workspace' },
      { title: 'Keyboard shortcuts', href: '/docs/shortcuts' },
    ],
  },
  {
    title: 'Editor',
    links: [
      { title: 'Markdown', href: '/docs/markdown' },
      { title: 'Preview', href: '/docs/preview' },
      { title: 'Files and workspaces', href: '/docs/files' },
      { title: 'Search', href: '/docs/search' },
      { title: 'Command palette', href: '/docs/command-palette' },
      { title: 'Frontmatter', href: '/docs/frontmatter' },
      { title: 'Images', href: '/docs/images' },
      { title: 'Tables', href: '/docs/tables' },
      { title: 'Code blocks', href: '/docs/code-blocks' },
    ],
  },
  {
    title: 'Configuration',
    links: [
      { title: 'Settings', href: '/docs/configuration' },
      { title: 'Themes', href: '/docs/themes' },
      { title: 'Keybindings', href: '/docs/keybindings' },
    ],
  },
  {
    title: 'Developer',
    links: [
      { title: 'CLI', href: '/docs/cli' },
      { title: 'Configuration files', href: '/docs/config-files' },
      { title: 'Architecture', href: '/docs/architecture' },
      { title: 'Building from source', href: '/docs/building' },
    ],
  },
  {
    title: 'Project',
    links: [
      { title: 'Roadmap', href: '/docs/roadmap' },
      { title: 'Contributing', href: '/docs/contributing' },
      { title: 'License', href: '/docs/license' },
    ],
  },
];

export function flattenDocsNav(): DocLink[] {
  return docsNav.flatMap((section) => section.links);
}
