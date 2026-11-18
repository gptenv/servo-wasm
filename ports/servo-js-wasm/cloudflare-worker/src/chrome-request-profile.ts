/**
 * Static Chrome 157 / Windows desktop request defaults generated with
 * header-generator@2.1.90. The package output is checked in so Worker requests
 * stay deterministic and do not depend on runtime randomness or Node APIs.
 * Chrome's Accept values below are selected per request destination.
 */
export const CHROME_REQUEST_PROFILE = Object.freeze({
  userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/157.0.0.0 Safari/537.36',
  secChUa: '"Not-A.Brand";v="99", "Google Chrome";v="157", "Chromium";v="157"',
  secChUaMobile: '?0',
  secChUaPlatform: '"Windows"',
  acceptLanguage: 'en-US,en;q=0.9',
  acceptEncoding: 'gzip, deflate, br, zstd',
  acceptDocument: 'text/html,application/xhtml+xml,application/xml;q=0.9,image/jxl,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7',
  acceptImage: 'image/avif,image/webp,image/apng,image/svg+xml,image/*,*/*;q=0.8',
  acceptStyle: 'text/css,*/*;q=0.1',
  acceptOther: '*/*',
});

const SERVO_DEFAULT_USER_AGENT = /\bServo\//i;
const SERVO_DEFAULT_ACCEPTS = new Set([
  'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
  'image/png,image/svg+xml,image/*;q=0.8,*/*;q=0.5',
  'application/json,*/*;q=0.5',
  'text/css,*/*;q=0.1',
  'text/plain,*/*;q=0.5',
  '*/*',
]);
const SERVO_DEFAULT_ACCEPT_LANGUAGES = new Set(['en-US']);

function normalizedDestination(destination: unknown, preflight: boolean): string {
  if (preflight) return 'empty';
  if (typeof destination !== 'string') return '';
  return destination === 'None' ? 'empty' : destination.toLowerCase();
}

function chromeAccept(destination: string): string {
  switch (destination) {
    case 'document':
    case 'iframe':
    case 'frame':
      return CHROME_REQUEST_PROFILE.acceptDocument;
    case 'image':
      return CHROME_REQUEST_PROFILE.acceptImage;
    case 'style':
      return CHROME_REQUEST_PROFILE.acceptStyle;
    default:
      return CHROME_REQUEST_PROFILE.acceptOther;
  }
}

/** Add defaults while retaining caller-supplied values and Servo's fetch metadata. */
export function applyChromeRequestDefaults(
  request: Request,
  requestContext: { destination?: unknown; preflight?: boolean } = {},
): Request {
  const url = new URL(request.url);
  const headers = new Headers(request.headers);
  const destination = normalizedDestination(
    requestContext.destination ?? headers.get('sec-fetch-dest'),
    requestContext.preflight === true,
  );
  const userAgent = headers.get('user-agent');
  if (userAgent === null || SERVO_DEFAULT_USER_AGENT.test(userAgent)) {
    headers.set('user-agent', CHROME_REQUEST_PROFILE.userAgent);
  }

  if (url.protocol === 'https:') {
    if (!headers.has('sec-ch-ua')) headers.set('sec-ch-ua', CHROME_REQUEST_PROFILE.secChUa);
    if (!headers.has('sec-ch-ua-mobile')) headers.set('sec-ch-ua-mobile', CHROME_REQUEST_PROFILE.secChUaMobile);
    if (!headers.has('sec-ch-ua-platform')) headers.set('sec-ch-ua-platform', CHROME_REQUEST_PROFILE.secChUaPlatform);
  }
  const acceptLanguage = headers.get('accept-language');
  if (acceptLanguage === null || SERVO_DEFAULT_ACCEPT_LANGUAGES.has(acceptLanguage)) {
    headers.set('accept-language', CHROME_REQUEST_PROFILE.acceptLanguage);
  }
  if (!headers.has('accept-encoding')) headers.set('accept-encoding', CHROME_REQUEST_PROFILE.acceptEncoding);

  const accept = headers.get('accept');
  if (accept === null || SERVO_DEFAULT_ACCEPTS.has(accept)) {
    headers.set('accept', requestContext.preflight ? CHROME_REQUEST_PROFILE.acceptOther : chromeAccept(destination));
  }
  if (url.protocol === 'https:' && !headers.has('sec-fetch-dest')) {
    headers.set('sec-fetch-dest', destination);
  }
  if (url.protocol === 'https:' && !headers.has('sec-fetch-mode')) {
    headers.set('sec-fetch-mode', requestContext.preflight ? 'cors' :
      ['document', 'iframe', 'frame'].includes(destination) ? 'navigate' :
        destination === 'empty' ? 'cors' : 'no-cors');
  }
  if (['document', 'iframe', 'frame'].includes(destination) &&
      !requestContext.preflight && !headers.has('upgrade-insecure-requests')) {
    headers.set('upgrade-insecure-requests', '1');
  }

  return new Request(request, { headers });
}
