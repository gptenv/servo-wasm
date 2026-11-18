import { describe, expect, it, vi } from 'vitest';
import { dedupeResults, parseBraveHtml, parseBingRss, parseDuckDuckGoHtml, parseGoogleHtml, parseYandexHtml, searchWeb } from '../src/web-search';

describe('search parsers', () => {
  it('parses Brave result cards and strips HTML from snippets', () => {
    const html = '<div class="snippet svelte-x" data-pos="0"><div class="result-content">' +
      '<a href="https://example.com/servo" target="_self"><div class="title search-snippet-title line-clamp-1" title="Servo &amp; WASM">Servo &amp; WASM</div></a>' +
      '<div class="generic-snippet"><div class="content desktop-default">A <strong>browser</strong> engine.</div></div>' +
      '</div></div>';
    expect(parseBraveHtml(html)).toEqual([
      { title: 'Servo & WASM', url: 'https://example.com/servo', snippet: 'A browser engine.', provider: 'brave' },
    ]);
  });

  it('parses Bing RSS and decodes XML entities', () => {
    const xml = '<rss><channel><item><title><![CDATA[Servo &amp; WASM]]></title><link>https://example.com/a?x=1&amp;y=2</link><description>A &lt;b&gt;browser&lt;/b&gt; &#39;result&#39;</description></item></channel></rss>';
    expect(parseBingRss(xml)).toEqual([
      { title: 'Servo & WASM', url: 'https://example.com/a?x=1&y=2', snippet: "A browser 'result'", provider: 'bing' },
    ]);
  });

  it('parses Google result links', () => {
    const html = '<a href="/url?q=https%3A%2F%2Fexample.com%2Fservo&sa=x"><h3>Servo</h3></a>';
    expect(parseGoogleHtml(html)).toEqual([{ title: 'Servo', url: 'https://example.com/servo', snippet: '', provider: 'google' }]);
  });

  it('parses DuckDuckGo results and snippets', () => {
    const html = '<a class="result__a" href="https://example.com/servo">Servo</a><div class="result__snippet">A browser engine.</div>';
    expect(parseDuckDuckGoHtml(html)).toEqual([{ title: 'Servo', url: 'https://example.com/servo', snippet: 'A browser engine.', provider: 'duckduckgo' }]);
  });

  it('parses Yandex result links', () => {
    const html = '<a href="https://example.com/servo">Servo</a>';
    expect(parseYandexHtml(html)).toEqual([{ title: 'Servo', url: 'https://example.com/servo', snippet: '', provider: 'yandex' }]);
  });

  it('deduplicates URLs while preserving result order', () => {
    const results = [
      { title: 'A', url: 'https://example.com/', snippet: '', provider: 'google' as const },
      { title: 'B', url: 'https://example.com', snippet: '', provider: 'bing' as const },
      { title: 'C', url: 'https://other.example', snippet: '', provider: 'yandex' as const },
    ];
    expect(dedupeResults(results, 10)).toHaveLength(2);
    expect(dedupeResults(results, 10)[0].title).toBe('A');
  });
});

describe("searchWeb via the caller's Servo HTML loader", () => {
  const searchFixture = async (url: URL): Promise<string> => {
    if (url.hostname === 'search.brave.com') {
      return '<div class="snippet"><a href="https://brave.example"><div class="title search-snippet-title" title="Brave result">Brave result</div></a><div class="content">Brave</div></div>';
    }
    if (url.hostname === 'www.google.com') {
      return '<html><a href="/url?q=https%3A%2F%2Fexample.com%2Fgoogle-result"><h3>Google result</h3></a></html>';
    }
    if (url.hostname === 'www.bing.com') {
      return '<rss><channel><item><title>Bing result</title><link>https://bing.example</link><description>Bing</description></item></channel></rss>';
    }
    if (url.hostname === 'html.duckduckgo.com') {
      return '<a class="result__a" href="https://ddg.example">DDG result</a><div class="result__snippet">DDG</div>';
    }
    throw new Error('Provider is unavailable');
  };

  it('validates the query before loading a browser document', async () => {
    const loader = vi.fn(searchFixture);
    await expect(searchWeb('   ', loader)).rejects.toThrow('must not be empty');
    await expect(searchWeb('x'.repeat(513), loader)).rejects.toThrow('exceeds 512');
    expect(loader).not.toHaveBeenCalled();
  });

  it('queries five providers sequentially inside the same Servo tab and tolerates failures', async () => {
    const order: string[] = [];
    let inFlight = 0;
    let concurrent = false;
    const loader = vi.fn(async (url: URL) => {
      if (++inFlight > 1) concurrent = true;
      order.push(url.hostname);
      await Promise.resolve();
      try {
        return await searchFixture(url);
      } finally {
        inFlight--;
      }
    });
    const workerFetch = vi.fn(() => { throw new Error('Search must not invoke Worker fetch'); });
    vi.stubGlobal('fetch', workerFetch);
    try {
      const result = await searchWeb('servo wasm', loader, 8, 'auto');
      expect(result.results.map((item) => item.provider)).toEqual(['brave', 'google', 'bing', 'duckduckgo']);
      expect(result.providers).toEqual(['brave', 'google', 'bing', 'duckduckgo']);
      expect(result.failedProviders).toEqual(['yandex']);
      expect(order).toEqual(['search.brave.com', 'www.google.com', 'www.bing.com', 'html.duckduckgo.com', 'yandex.com']);
      expect(concurrent).toBe(false);
      expect(workerFetch).not.toHaveBeenCalled();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('can target one search provider in a reusable browser', async () => {
    const loader = vi.fn(searchFixture);
    const result = await searchWeb('servo', loader, 2, 'bing');
    expect(result.results).toEqual([{ title: 'Bing result', url: 'https://bing.example', snippet: 'Bing', provider: 'bing' }]);
    expect(loader).toHaveBeenCalledTimes(1);
  });

  it('returns an empty result set when the browser loaded a blocked page', async () => {
    await expect(searchWeb('servo', async () => '<html>blocked</html>', 8, 'duckduckgo')).resolves.toEqual({
      results: [], providers: ['duckduckgo'], failedProviders: [],
    });
  });

  it('fails when all requested provider navigations fail', async () => {
    await expect(searchWeb('servo', async () => { throw new Error('navigation failed'); }, 8, 'all'))
      .rejects.toThrow('All selected web search providers failed');
  });
});
