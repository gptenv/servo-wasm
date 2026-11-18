export function assertPublicHttpUrl(value: string): URL {
  const url = new URL(value);
  if (url.protocol !== 'http:' && url.protocol !== 'https:') {
    throw new TypeError('Servo navigation only allows HTTP and HTTPS URLs.');
  }
  if (url.username || url.password) throw new TypeError('URLs containing credentials are not allowed.');
  const host = url.hostname.toLowerCase().replace(/^\[|\]$/g, '');
  const privateIpv4 = /^(?:0|10|127|169\.254|192\.168|224|240)\./.test(host) ||
    /^172\.(?:1[6-9]|2\d|3[01])\./.test(host) ||
    /^100\.(?:6[4-9]|[78]\d|9\d|1[01]\d|12[0-7])\./.test(host) ||
    /^198\.(?:18|19)\./.test(host) || /^192\.0\.0\./.test(host);
  const privateIpv6 = host === '::' || host === '::1' || host.startsWith('::ffff:') ||
    /^f[cd]/.test(host) || /^fe[89ab]/.test(host);
  if (host === 'localhost' || host.endsWith('.localhost') || host.endsWith('.local') ||
      host.endsWith('.internal') || host.endsWith('.test') || host.endsWith('.invalid') ||
      host === 'metadata.google.internal' || privateIpv4 || privateIpv6) {
    throw new TypeError('Private, local, and link-local network targets are blocked.');
  }
  return url;
}

export function assertPublicWebSocketUrl(value: string): URL {
  const url = new URL(value);
  if (url.protocol !== 'ws:' && url.protocol !== 'wss:') {
    throw new TypeError('Servo WebSockets only allow ws and wss URLs.');
  }
  const networkUrl = new URL(url.href);
  networkUrl.protocol = url.protocol === 'wss:' ? 'https:' : 'http:';
  assertPublicHttpUrl(networkUrl.href);
  return url;
}
