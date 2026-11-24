import adapter from '@sveltejs/adapter-static';

export default {
  kit: {
    adapter: adapter(),
    csp: {
      mode: 'hash',
      directives: {
        'default-src': ['self'],
        'script-src': ['self'],
        'style-src': ['self'],
        'connect-src': ['self'],
        'object-src': ['none'],
        'base-uri': ['none'],
        'form-action': ['none']
      }
    }
  }
};
