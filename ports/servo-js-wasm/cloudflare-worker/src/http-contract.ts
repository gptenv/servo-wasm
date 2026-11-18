import { z } from 'zod';

export const HTTP_METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'] as const;
export type HttpMethod = typeof HTTP_METHODS[number];

export const MAX_HTTP_REQUEST_BYTES = 1 * 1024 * 1024;
const MAX_RESPONSE_BYTES = 2 * 1024 * 1024;
const MAX_HEADERS = 64;

const headersSchema = z.record(z.string().min(1).max(256), z.string().max(8192))
  .refine((headers) => Object.keys(headers).length <= MAX_HEADERS, `At most ${MAX_HEADERS} headers are allowed.`);

export const httpRequestSchema = z.object({
  // Session options mirror the focused browser-action tools.
  sessionID: z.union([z.string().uuid(), z.literal(null), z.literal(false), z.literal(0), z.literal('')]).optional(),
  width: z.number().int().min(320).max(1920).optional(),
  height: z.number().int().min(240).max(1600).optional(),
  url: z.string().url().max(2048),
  method: z.string().regex(/^[!#$%&'*+.^_`|~0-9A-Za-z-]{1,32}$/).default('GET'),
  headers: headersSchema.optional(),
  body: z.string().max(MAX_HTTP_REQUEST_BYTES).optional(),
  followRedirects: z.boolean().default(true),
  maxResponseBytes: z.number().int().min(1).max(MAX_RESPONSE_BYTES).default(512 * 1024),
  maxDurationMs: z.number().int().min(100).max(15_000).default(10_000),
});

export const httpVerbRequestSchema = httpRequestSchema.omit({ method: true });
export type HttpRequest = z.infer<typeof httpRequestSchema>;

export type ServoHttpResponse = {
  status: number;
  statusText: string;
  url: string;
  redirected: boolean;
  headers: Record<string, string>;
  bodyBytes: number;
  truncated: boolean;
  body: Uint8Array;
};
