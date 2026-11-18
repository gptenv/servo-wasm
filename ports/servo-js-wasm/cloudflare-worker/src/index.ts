export { ServoBrowserSession } from './browser-session';

export default {
  fetch(): Response {
    return new Response('Not found', { status: 404 });
  },
};
