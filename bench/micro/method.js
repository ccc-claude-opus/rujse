class P { constructor() { this.v = 0; } inc() { this.v++; } }
function f(n) {
  const p = new P();
  for (let i = 0; i < n; i++) p.inc();
  return p.v;
}
if (f(__N__) !== __N__) throw new Error('method');
