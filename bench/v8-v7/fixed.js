// Fixed-work driver for wasmbench (instruction counts, not time): runs each
// benchmark's setup, its run function RUNS times, and its teardown.
(function () {
  var RUNS = typeof BENCH_RUNS === 'number' ? BENCH_RUNS : 3;
  for (var i = 0; i < BenchmarkSuite.suites.length; i++) {
    var suite = BenchmarkSuite.suites[i];
    for (var j = 0; j < suite.benchmarks.length; j++) {
      var b = suite.benchmarks[j];
      b.Setup();
      for (var k = 0; k < RUNS; k++) b.run();
      b.TearDown();
    }
  }
})();
