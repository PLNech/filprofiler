Further reduced memory overhead for tracking objects, at the cost of slightly lower resolution when tracking allocations >2GB.
Large allocations >2GB will only be accurate to a resoluion of ~1MB, i.e. they might be off by approximately 0.05%.
