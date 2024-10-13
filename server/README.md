# ORE Mining Pool Operator Server

## Docker

Build the docker image:
```bash
docker build -t ore-pool-server -f server/Dockerfile .
```

Run the docker image:

1. Generate a keypair for the pool authority:

```bash
solana-keygen new --outfile ./secrets/ore-pool-authority.json 
```

... do other stuff in root README.md ...

```bash
docker run -p 3000:3000 -it --rm ore-pool-server
```