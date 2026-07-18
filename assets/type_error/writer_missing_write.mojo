# expect: does not conform to trait 'Writer'
@fieldwise_init
struct BrokenWriter(Writer):
    var buffer: String
