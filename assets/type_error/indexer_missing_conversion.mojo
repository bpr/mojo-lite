# expect: does not conform to trait 'Indexer'
@fieldwise_init
struct BadIndex(Indexer):
    var value: Int

def main():
    var values = [1, 2]
    print(values[BadIndex(0)])
