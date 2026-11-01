// The pages of the site: where each is served, which file under content/ holds its <main>, what its head says and
// which scripts it loads. The content files are the page copy as written for the site (content/*.html: the inside of
// <main>); the document around them is layout.ts.

export type NavKey = 'concepts' | 'install' | 'kbf';

export interface Page {
  /** The URL path: '/' or '/name/' or '/404.html'. */
  route: string;
  /** content/<source>.html, the inside of <main>. */
  source: string;
  /** The <title> without the site suffix. */
  title: string;
  description: string;
  /** The navigation entry this page is. */
  nav?: NavKey;
  /** Browser modules (names under assets/<id>/) this page loads, in order. */
  scripts: readonly string[];
  /** The home runs in bands that each carry their own .wrap; every other page sits in one. */
  bands: boolean;
  noindex?: boolean;
}

export const SITE_NAME = 'jobscheduler';
export const DOMAIN = 'jobscheduler.dev';
export const REPO_URL = 'https://github.com/komira-ai/jobscheduler';
export const RELEASES_URL = `${REPO_URL}/releases`;

/** The navigation, in order: the label and the page it names. */
export const NAV: readonly { key: NavKey; label: string; route: string }[] = [
  { key: 'concepts', label: 'Concepts', route: '/concepts/' },
  { key: 'install', label: 'Install', route: '/install/' },
  { key: 'kbf', label: 'kbf', route: '/kbf/' },
];

export const PAGES: readonly Page[] = [
  {
    route: '/',
    source: 'index',
    title: 'Leases on whole hosts, placement by CPU and memory',
    description: 'jobscheduler is a scheduler for jobs and compute: a lease server, an agent on each host, and a pure placement core. Open source, Apache 2.0.',
    scripts: ['copy.js'],
    bands: true,
  },
  { route: '/concepts/', source: 'concepts', title: 'Concepts', description: 'Leases, whole-node leases and placement by CPU and memory, as the code has them.', nav: 'concepts', scripts: ['copy.js'], bands: false },
  { route: '/install/', source: 'install', title: 'Install', description: 'Get the lease server, the client and the agent from a release, and what the agent needs on a host.', nav: 'install', scripts: ['copy.js'], bands: false },
  { route: '/kbf/', source: 'kbf', title: 'kbf on jobscheduler', description: 'What kbf takes from jobscheduler today, and the design for running kbf actions on the agent, which is not built.', nav: 'kbf', scripts: ['copy.js'], bands: false },
  {
    route: '/404.html',
    source: '404',
    title: 'Page not found',
    description: 'There is no such page on jobscheduler.dev.',
    scripts: ['copy.js'],
    bands: false,
    noindex: true,
  },
];

/** The file a route is written to: '/' is index.html, '/a/b/' is a/b/index.html, '/404.html' is itself. */
export function outPath(route: string): string {
  const rel = route.replace(/^\//, '');
  return rel === '' || rel.endsWith('/') ? `${rel}index.html` : rel;
}

/**
 * The pages of the page copy name each other as '<source>.html' (the copy was written for a flat directory). Writes
 * those as the routes they are served at; an internal '.html' link that names no page is an error.
 */
export function rewriteLinks(html: string, pages: readonly Page[]): string {
  const bySource = new Map(pages.map((p) => [p.source, p.route]));
  return html.replace(/(\bhref=")([a-z0-9-]+)\.html(#[^"]*)?"/g, (_m, pre: string, name: string, frag: string | undefined) => {
    const route = bySource.get(name);
    if (route === undefined) throw new Error(`pages: a link to ${name}.html, which is no page`);
    return `${pre}${route}${frag ?? ''}"`;
  });
}
