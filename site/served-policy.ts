// The Content-Security-Policy jobscheduler.dev is to be served under. The site's infrastructure program, which sets
// the header for the sibling sites, is infra/site/lib/site-stack.ts in komira-ai/kbf; this is a copy of its
// CONTENT_SECURITY_POLICY, in the same form, which csp.test.ts checks the built pages against. Nothing in this
// repository serves the site yet. A change to the policy there is made here in the same step.
export const CONTENT_SECURITY_POLICY = [
  "default-src 'none'",
  "script-src 'self'",
  "style-src 'self'",
  "img-src 'self'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-ancestors 'none'",
  "object-src 'none'",
].join('; ');
