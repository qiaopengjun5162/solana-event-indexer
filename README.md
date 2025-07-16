# Solana event indexer

## Description

This is a simple indexer for Solana events. It listens to Solana events and stores them in a database.

## Setup

1. Install dependencies:

```bash
npm install
```

2. Create a `.env` file with the following content:

```bash
DB_HOST=localhost
DB_PORT=5432
DB_USER=postgres
DB_PASSWORD=postgres
DB_NAME=solana_events
```

3. Run the indexer:

```bash
npm run start
```

## Usage

The indexer listens to Solana events and stores them in a database. The database schema is defined in the `schema.sql` file.

## License

This project is licensed under the MIT License.

## Contributing

Contributions are welcome! Please feel free to submit pull requests or open issues.
