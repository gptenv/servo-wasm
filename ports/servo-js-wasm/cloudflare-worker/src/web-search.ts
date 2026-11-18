const DEFAULT_LIMIT = 8;
const MAX_LIMIT = 10;
const MAX_QUERY_LENGTH = 512;

export const SEARCH_PROVIDERS = ['auto', 'brave', 'google', 'bing', 'duckduckgo', 'yandex', 'all'] as const;
export type SearchProvider = typeof SEARCH_PROVIDERS[number];

export type WebSearchResult = {
  title: string;
  url: string;
  snippet: string;
  provider: Exclude<SearchProvider, 'auto' | 'all'>;
};

type Provider = Exclude<SearchProvider, 'auto' | 'all'>;

function decodeHtml(value: string): string {
  let text = value.replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, '$1');
  text = text.replace(/&(?:amp|lt|gt|quot|#39|apos);/g, (entity) => ({
    '&amp;': '&',
    '&lt;': '<',
    '&gt;': '>',
    '&quot;': '"',
    '&#39;': "'",
    '&apos;': "'",
  })[entity] ?? entity);
  text = text.replace(/&#(\d+);/g, (_, code) => String.fromCodePoint(Number(code)));
  text = text.replace(/&#x([0-9a-f]+);/gi, (_, code) => String.fromCodePoint(parseInt(code, 16)));
  return text.replace(/<[^>]*>/g, ' ').replace(/\s+/g, ' ').trim();
}
function decodeUrl(value: string): string {
  const decoded = decodeHtml(value);
  try { return decodeURIComponent(decoded); } catch { return decoded; }
}

function parseBingRss(xml: string, limit = DEFAULT_LIMIT): WebSearchResult[] {
  const items = xml.match(/<item[\s\S]*?<\/item>/gi) ?? [];
  return items.slice(0, limit).map((item) => ({
    title: decodeHtml((item.match(/<title[^>]*>([\s\S]*?)<\/title>/i)?.[1] ?? '')),
    url: decodeUrl(item.match(/<link[^>]*>([\s\S]*?)<\/link>/i)?.[1] ?? ''),
    snippet: decodeHtml(item.match(/<description[^>]*>([\s\S]*?)<\/description>/i)?.[1] ?? ''),
    provider: 'bing' as const,
  })).filter((result) => result.title && /^https?:\/\//i.test(result.url));
}

function parseBraveHtml(html: string, limit = DEFAULT_LIMIT): WebSearchResult[] {
  const blocks = html.match(/<div class="snippet\b[\s\S]*?(?=<div class="snippet\b|$)/gi) ?? [];
  const results: WebSearchResult[] = [];
  const seen = new Set<string>();
  for (const block of blocks) {
    const url = decodeUrl(block.match(/<a\s+href="(https?:\/\/[^"]+)"/i)?.[1] ?? '');
    const title = decodeHtml(block.match(/class="title search-snippet-title[^"]*"\s+title="([^"]*)"/i)?.[1] ?? '');
    const snippet = decodeHtml(block.match(/class="content[^"]*">([\s\S]*?)<\/div>/i)?.[1] ?? '');
    if (!title || !/^https?:\/\//i.test(url)) continue;
    let hostname: string;
    try { hostname = new URL(url).hostname; } catch { continue; }
    if (/search\.brave\.com$/i.test(hostname) || seen.has(url)) continue;
    seen.add(url);
    results.push({ title, url, snippet, provider: 'brave' });
    if (results.length >= limit) break;
  }
  return results;
}

function parseGoogleHtml(html: string, limit = DEFAULT_LIMIT): WebSearchResult[] {
  const results: WebSearchResult[] = [];
  const seen = new Set<string>();
  const pattern = /<a\b[^>]*\bhref="([^"]+)"[^>]*>([\s\S]*?)<\/a>/gi;
  for (const match of html.matchAll(pattern)) {
    const titleMatch = match[2].match(/<h3\b[^>]*>([\s\S]*?)<\/h3>/i);
    if (!titleMatch) continue;
    const rawUrl = decodeUrl(match[1]);
    const url = rawUrl.startsWith('/url?') ? new URL(rawUrl, 'https://www.google.com').searchParams.get('q') ?? '' : rawUrl;
    if (!url || !/^https?:\/\//i.test(url)) continue;
    let hostname: string;
    try { hostname = new URL(url).hostname; } catch { continue; }
    if (/google\./i.test(hostname)) continue;
    if (seen.has(url)) continue;
    seen.add(url);
    results.push({ title: decodeHtml(titleMatch[1]), url, snippet: '', provider: 'google' });
    if (results.length >= limit) break;
  }
  return results;
}
function parseDuckDuckGoHtml(html: string, limit = DEFAULT_LIMIT): WebSearchResult[] {
  const results: WebSearchResult[] = [];
  const pattern = /<a[^>]+class="[^"]*result__a[^"]*"[^>]+href="([^"]+)"[^>]*>([\s\S]*?)<\/a>([\s\S]*?)(?:<a[^>]+class="[^"]*result__snippet[^"]*"[^>]*>([\s\S]*?)<\/a>|<div[^>]+class="[^"]*result__snippet[^"]*"[^>]*>([\s\S]*?)<\/div>)/gi;
  for (const match of html.matchAll(pattern)) {
    const url = decodeUrl(match[1]);
    if (!/^https?:\/\//i.test(url)) continue;
    let hostname: string;
    try { hostname = new URL(url).hostname; } catch { continue; }
    if (/duckduckgo\./i.test(hostname)) continue;
    results.push({ title: decodeHtml(match[2]), url, snippet: decodeHtml(match[4] ?? match[5] ?? ''), provider: 'duckduckgo' });
    if (results.length >= limit) break;
  }
  return results;
}

function parseYandexHtml(html: string, limit = DEFAULT_LIMIT): WebSearchResult[] {
  const results: WebSearchResult[] = [];
  const seen = new Set<string>();
  const pattern = /<a[^>]+href="(https?:\/\/[^"]+)"[^>]*>([\s\S]*?)<\/a>/gi;
  for (const match of html.matchAll(pattern)) {
    const url = decodeUrl(match[1]);
    const title = decodeHtml(match[2]);
    if (!title || title.length < 2) continue;
    let hostname: string;
    try { hostname = new URL(url).hostname; } catch { continue; }
    if (/yandex\./i.test(hostname)) continue;
    if (seen.has(url)) continue;
    seen.add(url);
    results.push({ title, url, snippet: '', provider: 'yandex' });
    if (results.length >= limit) break;
  }
  return results;
}

export const parsers: Record<Provider, (body: string, limit?: number) => WebSearchResult[]> = {
  brave: parseBraveHtml,
  google: parseGoogleHtml,
  bing: parseBingRss,
  duckduckgo: parseDuckDuckGoHtml,
  yandex: parseYandexHtml,
};

const endpoints: Record<Provider, (query: string, limit: number) => URL> = {
  brave: (query) => {
    const url = new URL('https://search.brave.com/search');
    url.searchParams.set('q', query);
    return url;
  },
  google: (query, limit) => {
    const url = new URL('https://www.google.com/search');
    url.searchParams.set('q', query);
    url.searchParams.set('num', String(limit));
    url.searchParams.set('hl', 'en');
    return url;
  },
  bing: (query, limit) => {
    const url = new URL('https://www.bing.com/search');
    url.searchParams.set('q', query);
    url.searchParams.set('format', 'rss');
    url.searchParams.set('count', String(limit));
    url.searchParams.set('setlang', 'en-us');
    return url;
  },
  duckduckgo: (query) => {
    const url = new URL('https://html.duckduckgo.com/html/');
    url.searchParams.set('q', query);
    return url;
  },
  yandex: (query) => {
    const url = new URL('https://yandex.com/search/');
    url.searchParams.set('text', query);
    return url;
  },
};

export function dedupeResults(results: WebSearchResult[], limit = DEFAULT_LIMIT): WebSearchResult[] {
  const seen = new Set<string>();
  const unique: WebSearchResult[] = [];
  for (const result of results) {
    const key = result.url.replace(/#.*$/, '').replace(/\/$/, '');
    if (seen.has(key)) continue;
    seen.add(key);
    unique.push(result);
    if (unique.length >= limit) break;
  }
  return unique;
}

/** Search providers are loaded by the caller's real Servo browsing session. */
export type SearchHtmlLoader = (url: URL, provider: Provider) => Promise<string>;

async function searchProvider(
  provider: Provider, query: string, limit: number, loadHtml: SearchHtmlLoader,
): Promise<WebSearchResult[]> {
  return parsers[provider](await loadHtml(endpoints[provider](query, limit), provider), limit);
}

export async function searchWeb(
  query: string,
  loadHtml: SearchHtmlLoader,
  limit = DEFAULT_LIMIT,
  provider: SearchProvider = 'auto',
): Promise<{ results: WebSearchResult[]; providers: string[]; failedProviders: string[] }> {
  const trimmed = query.trim();
  if (!trimmed) throw new TypeError('Search query must not be empty.');
  if (trimmed.length > MAX_QUERY_LENGTH) throw new RangeError('Search query exceeds ' + MAX_QUERY_LENGTH + ' characters.');

  const count = Math.max(1, Math.min(MAX_LIMIT, Math.trunc(limit)));
  const selected: Provider[] = provider === 'all' || provider === 'auto'
    ? ['brave', 'google', 'bing', 'duckduckgo', 'yandex']
    : [provider];

  // A search action owns one tab. Providers navigate that tab in sequence,
  // while separate MCP actions execute concurrently in distinct tabs.
  // Never load providers concurrently into the same browsing context.
  const successful: WebSearchResult[] = [];
  const providers: string[] = [];
  const failedProviders: string[] = [];
  for (const name of selected) {
    try {
      const matches = await searchProvider(name, trimmed, count, loadHtml);
      providers.push(name);
      successful.push(...matches);
    } catch {
      failedProviders.push(name);
    }
  }

  if (successful.length === 0 && providers.length === 0) {
    throw new Error('All selected web search providers failed: ' + failedProviders.join(', ') + '.');
  }
  return { results: dedupeResults(successful, count), providers, failedProviders };
}

export { parseBraveHtml, parseBingRss, parseGoogleHtml, parseDuckDuckGoHtml, parseYandexHtml };
