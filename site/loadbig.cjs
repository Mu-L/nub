const t0 = process.hrtime.bigint();
require(process.env.TARGET);
console.error("TOTAL_MS=" + (Number(process.hrtime.bigint()-t0)/1e6).toFixed(1) + " OK");
