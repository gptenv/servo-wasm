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
];
