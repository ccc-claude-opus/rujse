function counter() { let c = 0; return () => ++c; }
function f(n) {
  const next = counter();
  let v = 0;
  for (let i = 0; i < n; i++) v = next();
  return v;
}
if (f(__N__) !== __N__) throw new Error('closure');
