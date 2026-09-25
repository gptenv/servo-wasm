// Small, original, deterministic web-platform-style cases. These are not an
// upstream WPT runner or a claim of complete browser conformance. Each case
// runs in the current page and leaves its temporary DOM subtree disconnected.
export const webPlatformCases = [
  {
    name: 'HTML templates, entity decoding, and selector matching',
    source: `(() => {
      const template = document.createElement('template');
      template.innerHTML = '<section class="card"><span data-v="a&amp;b">💡 &lt;ok&gt;</span></section>';
      const span = template.content.querySelector('section.card > span[data-v="a&b"]');
      return span.textContent === '💡 <ok>' && span.closest('section').matches('.card');
    })()`,
  },
  {
    name: 'DOM fragments, clone independence, and child mutation',
    source: `(() => {
      const parent = document.createElement('div');
      const fragment = document.createDocumentFragment();
      const child = document.createElement('i');
      child.textContent = 'original';
      fragment.append(child);
      parent.append(fragment);
      const clone = parent.cloneNode(true);
      clone.firstChild.textContent = 'copy';
      return fragment.childNodes.length === 0 && parent.firstChild === child &&
        child.parentNode === parent && parent.textContent === 'original' && clone.textContent === 'copy';
    })()`,
  },
  {
    name: 'event capture, target, bubbling, once, and cancellation',
    source: `(() => {
      const parent = document.createElement('div');
      const child = document.createElement('button');
      parent.append(child);
      const seen = [];
      parent.addEventListener('probe', () => seen.push('capture'), true);
      parent.addEventListener('probe', () => seen.push('bubble'));
      child.addEventListener('probe', e => { seen.push('target'); e.preventDefault(); }, {once:true});
      const first = child.dispatchEvent(new Event('probe', {bubbles:true, cancelable:true}));
      const second = child.dispatchEvent(new Event('probe', {bubbles:true, cancelable:true}));
      return !first && second && seen.join(',') === 'capture,target,bubble,capture,bubble';
    })()`,
  },
  {
    name: 'CSS rule insertion and computed-style invalidation',
    source: `(() => {
      const style = document.createElement('style');
      const node = document.createElement('div');
      node.id = 'platform-style-probe';
      document.head.append(style);
      document.body.append(node);
      try {
        style.sheet.insertRule('#platform-style-probe { color: rgb(1, 2, 3) }', 0);
        const before = getComputedStyle(node).color;
        node.style.color = 'rgb(4, 5, 6)';
        const after = getComputedStyle(node).color;
        style.sheet.deleteRule(0);
        return before === 'rgb(1, 2, 3)' && after === 'rgb(4, 5, 6)' && style.sheet.cssRules.length === 0;
      } finally { node.remove(); style.remove(); }
    })()`,
  },
  {
    name: 'shadow DOM keeps tree-scoped selectors separate',
    source: `(() => {
      const host = document.createElement('div');
      const shadow = host.attachShadow({mode:'open'});
      shadow.innerHTML = '<span class="inside">shadow</span>';
      document.body.append(host);
      try {
        return host.shadowRoot === shadow && host.querySelector('.inside') === null &&
          shadow.querySelector('.inside').getRootNode() === shadow;
      } finally { host.remove(); }
    })()`,
  },
  {
    name: 'custom elements upgrade existing nodes and run connected callbacks',
    source: `(() => {
      const name = 'worker-probe-' + Math.random().toString(36).slice(2);
      const existing = document.createElement(name);
      document.body.append(existing);
      let connected = 0;
      class ProbeElement extends HTMLElement {
        connectedCallback() { connected++; this.dataset.upgraded = 'yes'; }
      }
      customElements.define(name, ProbeElement);
      try {
        return existing instanceof ProbeElement && connected === 1 &&
          existing.dataset.upgraded === 'yes' && customElements.get(name) === ProbeElement;
      } finally { existing.remove(); }
    })()`,
  },
  {
    name: 'MutationObserver batches attribute and child-list records',
    source: `(() => {
      const node = document.createElement('div');
      const seen = [];
      const observer = new MutationObserver(records => {
        for (const record of records) seen.push(record.type + ':' + record.attributeName);
      });
      observer.observe(node, {attributes:true, childList:true});
      node.setAttribute('data-probe', 'yes');
      node.append(document.createElement('span'));
      const records = observer.takeRecords();
      observer.disconnect();
      return records.length === 2 && records[0].type === 'attributes' &&
        records[0].attributeName === 'data-probe' && records[1].type === 'childList' &&
        seen.length === 0;
    })()`,
  },
  {
    name: 'canvas 2D fills, transforms, and exact getImageData readback',
    source: `(() => {
      const c = document.createElement('canvas'); c.width = 8; c.height = 8;
      const ctx = c.getContext('2d');
      if (!ctx) return false;
      ctx.fillStyle = 'rgb(255, 0, 0)'; ctx.fillRect(0, 0, 4, 4);
      ctx.fillStyle = '#00ff00'; ctx.translate(4, 4); ctx.fillRect(0, 0, 4, 4);
      const px = (x, y) => Array.from(ctx.getImageData(x, y, 1, 1).data).join(',');
      return px(1, 1) === '255,0,0,255' && px(6, 6) === '0,255,0,255' && px(6, 1) === '0,0,0,0';
    })()`,
  },
  {
    name: 'canvas 2D clipping and putImageData',
    source: `(() => {
      const c = document.createElement('canvas'); c.width = 10; c.height = 10;
      const ctx = c.getContext('2d');
      ctx.beginPath(); ctx.rect(0, 0, 5, 10); ctx.clip();
      ctx.fillStyle = 'blue'; ctx.fillRect(0, 0, 10, 10);
      const inside = ctx.getImageData(2, 5, 1, 1).data;
      const outside = ctx.getImageData(7, 5, 1, 1).data;
      const img = ctx.createImageData(1, 1); img.data.set([10, 20, 30, 255]);
      ctx.putImageData(img, 8, 8);
      return inside[2] === 255 && inside[3] === 255 && outside[3] === 0 &&
        Array.from(ctx.getImageData(8, 8, 1, 1).data).join(',') === '10,20,30,255';
    })()`,
  },
  {
    name: 'canvas 2D drawImage between canvases, patterns, and gradients',
    source: `(() => {
      const src = document.createElement('canvas'); src.width = 2; src.height = 2;
      const s = src.getContext('2d'); s.fillStyle = 'rgb(0, 0, 255)'; s.fillRect(0, 0, 2, 2);
      const dst = document.createElement('canvas'); dst.width = 6; dst.height = 6;
      const d = dst.getContext('2d');
      d.drawImage(src, 0, 0);
      d.fillStyle = d.createPattern(src, 'repeat'); d.fillRect(4, 4, 2, 2);
      const g = d.createLinearGradient(2, 0, 4, 0);
      g.addColorStop(0, '#f00'); g.addColorStop(1, '#f00');
      d.fillStyle = g; d.fillRect(2, 0, 2, 2);
      const px = (x, y) => Array.from(d.getImageData(x, y, 1, 1).data).join(',');
      return px(1, 1) === '0,0,255,255' && px(5, 5) === '0,0,255,255' &&
        px(3, 1) === '255,0,0,255' && px(1, 4) === '0,0,0,0';
    })()`,
  },
  {
    name: 'canvas toDataURL encodes a PNG',
    source: `(() => {
      const c = document.createElement('canvas'); c.width = 3; c.height = 3;
      const ctx = c.getContext('2d'); ctx.fillStyle = '#123456'; ctx.fillRect(0, 0, 3, 3);
      const url = c.toDataURL();
      return url.startsWith('data:image/png;base64,iVBORw0KGgo') && url.length > 40;
    })()`,
  },
  {
    name: 'no-UI Worker answers: dialogs dismissed, popups blocked, screen is the viewport',
    source: `(() => {
      return alert('x') === undefined && confirm('x') === false && prompt('x', 'd') === null &&
        window.open('about:blank') === null && history.length === 1 &&
        screen.width === window.innerWidth && screen.availHeight === window.innerHeight &&
        window.outerWidth === window.innerWidth && window.screenX === 0;
    })()`,
  },
  {
    name: 'localStorage and sessionStorage round-trip in memory',
    source: `(() => {
      localStorage.clear(); sessionStorage.clear();
      localStorage.setItem('k', 'v'); sessionStorage.setItem('s', '1');
      const ok = localStorage.getItem('k') === 'v' && localStorage.length === 1 &&
        localStorage.key(0) === 'k' && sessionStorage.getItem('s') === '1' &&
        localStorage.getItem('s') === null && Object.keys(localStorage).includes('k');
      localStorage.removeItem('k');
      return ok && localStorage.getItem('k') === null && localStorage.length === 0;
    })()`,
  },
  {
    name: 'text layout uses real font metrics (bundled fonts)',
    source: `(() => {
      const span = document.createElement('span');
      span.style.font = '16px sans-serif';
      span.textContent = 'Hello, world';
      const mono = document.createElement('span');
      mono.style.font = '16px monospace';
      document.body.append(span, mono);
      try {
        const box = span.getBoundingClientRect();
        mono.textContent = 'iii';
        const narrow = mono.getBoundingClientRect().width;
        mono.textContent = 'WWW';
        const wide = mono.getBoundingClientRect().width;
        return box.width > 50 && box.width < 150 && box.height > 10 &&
          narrow > 0 && Math.abs(narrow - wide) < 0.01;
      } finally { span.remove(); mono.remove(); }
    })()`,
  },
  {
    name: 'canvas text: measureText, fillText ink, and non-ASCII shaping',
    source: `(() => {
      const c = document.createElement('canvas'); c.width = 80; c.height = 24;
      const ctx = c.getContext('2d');
      ctx.font = '16px sans-serif';
      const hello = ctx.measureText('Hello').width;
      const narrow = ctx.measureText('iii').width;
      const wide = ctx.measureText('WWW').width;
      const accented = ctx.measureText('Crème brûlée').width;
      ctx.fillStyle = '#000';
      ctx.fillText('Hello', 2, 18);
      const data = ctx.getImageData(0, 0, 80, 24).data;
      let inked = 0;
      for (let i = 3; i < data.length; i += 4) if (data[i]) inked++;
      return hello > 20 && hello < 60 && wide > narrow && accented > hello && inked > 20;
    })()`,
  },
];
