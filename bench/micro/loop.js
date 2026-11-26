// The loop measured in rubrowser's Performance panel (~2,300 fuel per
// iteration with boa).
var x = 0;
for (var k = 0; k < __N__; k++) x += k;
if (x !== __N__ * (__N__ - 1) / 2) throw new Error('loop: ' + x);
