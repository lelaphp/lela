/* The fixture's stand-in for php-src's ext/random/php_random.h at the
 * pinned tag: the default engine's declarations, copied as written. */
PHPAPI const php_random_algo *php_random_default_algo(void);
PHPAPI void *php_random_default_status(void);

static inline php_random_algo_with_state php_random_default_engine(void)
{
	php_random_algo_with_state raws;
	raws.state = php_random_default_status();
	raws.algo = php_random_default_algo();
	return raws;
}
