var v: SIMD[DType.float64, 4] = SIMD[DType.float64, 4](1.0, 2.0, 3.0, 4.0)
var scaled: SIMD[DType.float64, 4] = v * 2.0
var lane: Float64 = scaled[3]
print(lane)
